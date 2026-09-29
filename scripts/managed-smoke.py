#!/usr/bin/env python3
"""End-to-end smoke of the managed path on the local Docker or Podman engine.

Builds the images, brings up deploy/compose, deploys examples/local through management's API and plays it through the
edge with two offline bots. It then checks that JVM machines ran the runner image and that their capacity was released,
deletes the environment and takes the bundle down, and checks that nothing it created remains. Logs go to a temporary
directory, whose path is printed. Needs `just toolchain` first, and a compose provider for Podman.
"""
import hashlib
import io
import json
from pathlib import Path
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tarfile
import tempfile
import time
import tomllib
import urllib.error
import urllib.request
import uuid

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent / 'smoke'))
import bot  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]
BUNDLE = ROOT / 'deploy/compose'
PROJECT = 'chunk-staging-managed-smoke'
NETWORK = 'chunk'
TAG = 'managed-smoke'
# Workload.JVM, and CapacityState.READY and RELEASED, as management stores them.
JVM, READY, RELEASED = 1, 2, 4
# The bots log in offline, which the bundle never allows.
OVERRIDE = """\
services:
  management:
    environment:
      CHUNK_MACHINE_OFFLINE_LOGINS: "1"
"""


class Failed(Exception):
    pass


def step(message):
    print(f'[{time.strftime("%H:%M:%S")}] {message}', flush=True)


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    if port == 25565:
        return free_port()
    return port


def wait_for(check, label, timeout, interval=2):
    deadline = time.monotonic() + timeout
    while True:
        result = check()
        if result:
            return result
        if time.monotonic() > deadline:
            raise Failed(f'timed out after {timeout}s waiting for {label}')
        time.sleep(interval)


class Smoke:
    def __init__(self):
        self.engine = 'podman' if shutil.which('podman') else 'docker'
        self.work = Path(tempfile.mkdtemp(prefix='chunk-managed-smoke-'))
        self.logs = self.work / 'logs'
        self.logs.mkdir()
        self.env_file = self.work / '.env'
        self.override = self.work / 'compose.override.yaml'
        self.compose = [self.engine, 'compose', '-p', PROJECT, '--env-file', str(self.env_file),
                        '-f', str(BUNDLE / 'compose.yaml'), '-f', str(self.override)]
        self.player_port = free_port()
        self.management_port = free_port()
        self.url = f'http://127.0.0.1:{self.management_port}'
        self.token = None
        self.environment = None
        self.archive = None
        self.up = False
        self.timings = {}

    def run(self, argv, log=None, timeout=1800, check=True, **kwargs):
        if log:
            with (self.logs / log).open('w') as out:
                result = subprocess.run(argv, stdout=out, stderr=subprocess.STDOUT, timeout=timeout, **kwargs)
        else:
            result = subprocess.run(argv, capture_output=True, text=True, timeout=timeout, **kwargs)
        if check and result.returncode != 0:
            detail = f'see {self.logs / log}' if log else (result.stderr or result.stdout).strip()
            raise Failed(f'{" ".join(map(str, argv[:4]))} exited {result.returncode}: {detail}')
        return result

    def timed(self, name, work):
        start = time.monotonic()
        try:
            return work()
        finally:
            self.timings[name] = time.monotonic() - start

    # Management's API, over Connect JSON with the operator token.
    def rpc(self, method, body):
        request = urllib.request.Request(
            f'{self.url}/chunk.management.v1.{method}', data=json.dumps(body).encode(), method='POST',
            headers={'authorization': f'Bearer {self.token}', 'content-type': 'application/json'})
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            raise Failed(f'{method}: HTTP {error.code} {error.read().decode(errors="replace")}') from None

    def sql(self, query):
        result = self.run([*self.compose, 'exec', '-T', 'postgres', 'psql', '-U', 'chunk', '-d', 'chunk',
                           '-AtF', '\t', '-c', query])
        return [line.split('\t') for line in result.stdout.splitlines() if line]

    def machines(self, workload=None):
        """This environment's machines, running or not, as (name, image, state)."""
        filters = ['--filter', f'label=chunk.environment={self.environment}']
        if workload:
            filters += ['--filter', f'label=chunk.workload={workload}']
        result = self.run([self.engine, 'ps', '-a', *filters, '--format', '{{.Names}}\t{{.Image}}\t{{.State}}'])
        return [line.split('\t') for line in result.stdout.splitlines() if line]

    def owned(self):
        """Every container, volume and network this run's names mark as its own."""
        prefixes = [PROJECT]
        if self.environment:
            prefixes.append('chunk-' + self.environment.replace('_', '-'))
        kinds = {
            'container': [self.engine, 'ps', '-a', '--format', '{{.Names}}'],
            'volume': [self.engine, 'volume', 'ls', '--format', '{{.Name}}'],
            'network': [self.engine, 'network', 'ls', '--format', '{{.Name}}'],
        }
        found = []
        for kind, argv in kinds.items():
            for name in self.run(argv).stdout.split():
                if name.startswith(tuple(prefixes)) or (kind == 'network' and name == NETWORK and self.up):
                    found.append((kind, name))
        return found

    def preflight(self):
        """Refuses to run next to another install or an earlier run's leftovers, which cleanup would take down."""
        if self.run([self.engine, 'network', 'inspect', NETWORK], check=False).returncode == 0:
            raise Failed(f'a `{NETWORK}` network already exists, so another chunk install may be running')
        if self.owned():
            raise Failed(f'resources from an earlier run remain: {self.owned()}')

    def prepare(self):
        step(f'building examples/local into {self.work / "release"}')
        chunk = ROOT / 'target/debug/chunk'
        if not chunk.exists():
            raise Failed('target/debug/chunk is missing; run `just toolchain`')
        self.run([str(chunk), 'build', str(ROOT / 'examples/local'), '--output', str(self.work / 'release')],
                 'build-release.log')
        [self.archive] = (self.work / 'release').glob('*.tar.gz')
        with tarfile.open(self.archive) as archive:
            self.java = json.load(archive.extractfile('release.json'))['java_version']

        version = tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']['package']['version']
        revision = self.run(['git', '-C', str(ROOT), 'rev-parse', 'HEAD']).stdout.strip()
        self.jvm_image = f'chunk-jvm:{TAG}-{self.java}'
        images = [
            ('crates/chunk-environment/Dockerfile', f'chunk-environment:{TAG}', []),
            ('crates/chunk-jvm/Dockerfile', self.jvm_image, ['--build-arg', f'JAVA_VERSION={self.java}']),
            ('crates/chunk-edge/Dockerfile', f'chunk-edge:{TAG}', []),
            ('packages/management/Dockerfile', f'chunk-management:{TAG}', []),
        ]
        for dockerfile, image, extra in images:
            step(f'building {image}')
            self.run([self.engine, 'build', '-f', dockerfile, '-t', image, '--build-arg', f'VERSION={version}',
                      '--build-arg', f'REVISION={revision}', *extra, '.'],
                     f'image-{image.split(":")[0]}.log', cwd=ROOT)

    def start(self):
        self.run([str(BUNDLE / 'init.sh'), str(self.env_file)])
        settings = {
            'CHUNK_PLAYER_BIND': '127.0.0.1',
            'CHUNK_PLAYER_PORT': self.player_port,
            'CHUNK_MANAGEMENT_PUBLISH': f'127.0.0.1:{self.management_port}',
            'CHUNK_PUBLIC_URL': self.url,
            'CHUNK_ENVIRONMENT_IMAGE': f'chunk-environment:{TAG}',
            'CHUNK_JVM_IMAGE': f'chunk-jvm:{TAG}-{{java}}',
            'CHUNK_EDGE_IMAGE': f'chunk-edge:{TAG}',
            'CHUNK_MANAGEMENT_IMAGE': f'chunk-management:{TAG}',
        }
        with self.env_file.open('a') as env:
            env.writelines(f'{name}={value}\n' for name, value in settings.items())
        self.override.write_text(OVERRIDE)
        self.token = next(line.split('=', 1)[1] for line in self.env_file.read_text().splitlines()
                          if line.startswith('CHUNK_OPERATOR_TOKEN='))
        step(f'compose up: players on 127.0.0.1:{self.player_port}, management at {self.url}')
        self.up = True
        self.run([*self.compose, 'up', '-d', '--wait', '--wait-timeout', '180'], 'compose-up.log', timeout=300)

    def deploy(self):
        project = self.rpc('ProjectService/CreateProject', {'requestId': str(uuid.uuid4()), 'name': 'smoke'})['project']
        environment = self.rpc('ProjectService/CreateEnvironment', {
            'requestId': str(uuid.uuid4()), 'projectId': project['id'], 'name': 'staging'})['environment']
        self.environment = environment['id']
        self.hostname = environment.get('hostname')
        if not self.hostname:
            raise Failed('the environment has no hostname; is CHUNK_EDGE_DOMAIN set?')
        release_id = self.archive.name.removesuffix('.tar.gz')
        data = self.archive.read_bytes()
        upload = self.rpc('DeploymentService/UploadRelease', {
            'projectId': project['id'], 'releaseId': release_id,
            'archiveSha256': hashlib.sha256(data).hexdigest(), 'archiveSizeBytes': str(len(data))})
        target = upload['upload']
        put = urllib.request.Request(target['url'], data=data, method=target.get('method', 'PUT'),
                                     headers=target.get('headers', {}))
        with urllib.request.urlopen(put, timeout=300):
            pass
        release = self.rpc('DeploymentService/CompleteReleaseUpload',
                           {'projectId': project['id'], 'releaseId': release_id})['release']
        if release['state'] != 'RELEASE_STATE_READY':
            raise Failed(f'release {release_id} is {release["state"]} after upload')
        step(f'deploying release {release_id[:12]} to {self.environment} ({self.hostname})')
        deployment = self.rpc('DeploymentService/Deploy', {
            'requestId': str(uuid.uuid4()), 'environmentId': self.environment, 'releaseId': release_id})['deployment']

        def active():
            current = self.rpc('DeploymentService/GetDeployment', {'deploymentId': deployment['id']})['deployment']
            if current['state'] == 'DEPLOYMENT_STATE_FAILED':
                raise Failed(f'deployment failed: {current.get("message", "")}')
            return current['state'] == 'DEPLOYMENT_STATE_ACTIVE'
        wait_for(active, 'the deployment to become active', 600)
        wait_for(lambda: self.rpc('ProjectService/GetEnvironment', {'environmentId': self.environment})
                 ['environment']['state'] == 'ENVIRONMENT_STATE_RUNNING', 'the environment to run', 120)
        wait_for(self.status, 'a status response through the edge', 120)

    def status(self):
        """Whether the edge routes a status request for the environment's hostname to its backend."""
        try:
            with socket.create_connection(('127.0.0.1', self.player_port), timeout=5) as sock:
                packet = (b'\0' + bot.varint(bot.PROTOCOL) + bot.string(self.hostname)
                          + struct.pack('>H', self.player_port) + b'\x01')
                sock.sendall(bot.varint(len(packet)) + packet + b'\x01\x00')
                stream = sock.makefile('rb')
                frame = stream.read(bot.read_varint(stream))
        except (OSError, EOFError):
            return False
        data = io.BytesIO(frame)
        if bot.read_varint(data) != 0:
            return False
        response = json.loads(data.read(bot.read_varint(data)))
        (self.logs / 'status.json').write_text(json.dumps(response, indent=2) + '\n')
        return response['version']['protocol'] == bot.PROTOCOL and 'chunk typed backend' in json.dumps(response)

    def jvm_requests(self):
        return self.sql(f"select request_id, app_id, state from capacity_requests "
                        f"where environment_id = '{self.environment}' and workload = {JVM} order by create_time")

    def play(self):
        step('running two offline bots through the edge')
        with (self.logs / 'bot.log').open('w') as out:
            code = subprocess.run([sys.executable, str(ROOT / 'scripts/smoke/bot.py'), '--port', str(self.player_port),
                                   '--host', self.hostname], stdout=out, stderr=subprocess.STDOUT, timeout=600).returncode
        self.dump_logs()
        if code:
            raise Failed(f'the bot scenario failed; see {self.logs / "bot.log"}')
        requests = self.jvm_requests()
        if {app for _, app, _ in requests} != {'lobby', 'arena'} or any(int(s) != READY for *_, s in requests):
            raise Failed(f'expected READY JVM capacity for lobby and arena, got {requests}')
        jvms = self.machines('jvm')
        wanted = {self.jvm_image, f'localhost/{self.jvm_image}', f'docker.io/library/{self.jvm_image}'}
        if len(jvms) != len(requests) or any(image not in wanted or state != 'running' for _, image, state in jvms):
            raise Failed(f'expected {len(requests)} running {self.jvm_image} machines, got {jvms}')
        step(f'PASS: {len(jvms)} JVM machines run {self.jvm_image}; their capacity is READY')

    def release(self):
        step('waiting for idle JVMs to be retired and their capacity RELEASED')
        wait_for(lambda: all(int(s) == RELEASED for *_, s in self.jvm_requests()),
                 'JVM capacity to be released', 240, interval=5)
        wait_for(lambda: not self.machines('jvm'), 'released JVM machines to be removed', 60)
        step('PASS: JVM capacity RELEASED and its machines removed')

    def delete(self, timeout=180):
        self.rpc('ProjectService/DeleteEnvironment', {'environmentId': self.environment})

        def gone():
            try:
                self.rpc('ProjectService/GetEnvironment', {'environmentId': self.environment})
                return False
            except Failed as error:
                return 'HTTP 404' in str(error) and not self.machines()
        wait_for(gone, 'the environment and its machines to be deleted', timeout)

    def dump_logs(self):
        try:
            self.run([*self.compose, 'logs', '--no-color', '--timestamps'], 'compose.log', check=False, timeout=60)
            if self.environment:
                for name, image, _ in self.machines():
                    self.run([self.engine, 'logs', '--timestamps', name], f'{name}.log', check=False, timeout=60)
                (self.logs / 'capacity.tsv').write_text('\n'.join('\t'.join(row) for row in self.sql(
                    'select request_id, workload, app_id, state, torn_down, message from capacity_requests')) + '\n')
        except Exception as error:
            print(f'collecting logs failed: {error}', file=sys.stderr)

    def cleanup(self, passed):
        """Deletes the environment, takes the bundle down and removes anything left; True if nothing was left."""
        if self.up and self.environment:
            try:
                self.delete(timeout=90)
            except Exception as error:
                print(f'cleanup: deleting the environment failed: {error}', file=sys.stderr)
        if self.up:
            self.run([*self.compose, 'down', '-v', '--remove-orphans', '--timeout', '20'], 'compose-down.log',
                     check=False, timeout=300)
        leftovers = self.owned()
        for kind, name in leftovers:
            argv = {'container': ['rm', '-f', '-v'], 'volume': ['volume', 'rm', '-f'], 'network': ['network', 'rm']}[kind]
            self.run([self.engine, *argv, name], check=False)
        if leftovers:
            print(f'FAIL: left behind after cleanup, now removed: {leftovers}', file=sys.stderr)
        # The install is gone, so its secrets are useless; a failed run keeps its release for a rerun by hand.
        self.env_file.unlink(missing_ok=True)
        if passed:
            shutil.rmtree(self.work / 'release', ignore_errors=True)
        return not leftovers


def interrupted(signum, frame):
    raise KeyboardInterrupt


def main():
    signal.signal(signal.SIGTERM, interrupted)
    smoke = Smoke()
    try:
        smoke.preflight()
    except Failed as error:
        print(f'STOPPED: {error}; nothing was changed', file=sys.stderr)
        shutil.rmtree(smoke.work)
        return 2
    step(f'logs: {smoke.logs}')
    passed = False
    try:
        smoke.timed('build', smoke.prepare)
        smoke.timed('compose up', smoke.start)
        smoke.timed('deploy', smoke.deploy)
        step('PASS: deployed through the API; the edge routes the hostname to the backend')
        smoke.timed('bots', smoke.play)
        smoke.timed('release', smoke.release)
        smoke.timed('delete', smoke.delete)
        step('PASS: environment deleted with its machines')
        passed = True
    except (Exception, KeyboardInterrupt) as error:
        print(f'FAIL: {type(error).__name__}: {error}', file=sys.stderr)
        smoke.dump_logs()
    finally:
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        try:
            clean = smoke.timed('cleanup', lambda: smoke.cleanup(passed))
        except Exception as error:
            print(f'FAIL: cleanup: {error}', file=sys.stderr)
            clean = False
    if clean:
        step('PASS: no containers, volumes or networks from this run remain')
    print('timings: ' + ', '.join(f'{name} {seconds:.0f}s' for name, seconds in smoke.timings.items()))
    print(f'{"PASS" if passed and clean else "FAIL"}; logs in {smoke.logs}')
    return 0 if passed and clean else 1


if __name__ == '__main__':
    sys.exit(main())
