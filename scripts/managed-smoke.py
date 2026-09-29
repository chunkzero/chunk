#!/usr/bin/env python3
"""End-to-end smoke of the managed path on the local Podman or Docker engine.

Builds the images, brings up deploy/compose, deploys examples/local through management's API and plays it through the
edge with two offline bots. It then checks that JVM machines ran the runner image and that their capacity was released,
deletes the environment and takes the bundle down. Cleanup removes only what this run recorded creating, and checks that
none of it remains, except the build cache: image layers and pulled base images stay for the next run. Runs on one engine
at a time, fenced by the `chunk-managed-smoke-lock` network. Logs go to a temporary directory, whose path is printed.
Needs `just toolchain` first, and a compose provider for Podman.
"""
import argparse
import fcntl
import hashlib
import io
import json
import os
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
PROJECT_LABEL = f'com.docker.compose.project={PROJECT}'
NETWORK = 'chunk'
ENGINE_LOCK = 'chunk-managed-smoke-lock'
TAG = 'managed-smoke'
RUNTIME_DIR = os.environ.get('XDG_RUNTIME_DIR') or f'/run/user/{os.getuid()}'
LOCK = Path(RUNTIME_DIR if Path(RUNTIME_DIR).is_dir() else tempfile.gettempdir()) / 'chunk-managed-smoke.lock'
# Workload.JVM, and CapacityState.READY and RELEASED, as management stores them.
JVM, READY, RELEASED = 1, 2, 4
LISTINGS = {
    'container': ('ps', '-a', '-q', '--no-trunc'),
    'volume': ('volume', 'ls', '-q'),
    'network': ('network', 'ls', '-q', '--no-trunc'),
}
REMOVALS = {'container': ('rm', '-f', '-v'), 'volume': ('volume', 'rm', '-f'), 'network': ('network', 'rm')}
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


def choose_engine(name):
    """The engine CLI, the socket management drives the same engine through, and that socket's group."""
    podman = Path(RUNTIME_DIR, 'podman/podman.sock')
    docker = Path('/var/run/docker.sock')
    if name in (None, 'podman') and shutil.which('podman') and podman.is_socket():
        return 'podman', podman, 0
    if name in (None, 'docker') and shutil.which('docker') and docker.is_socket():
        return 'docker', docker, docker.stat().st_gid
    raise Failed(f'found no {name or "Podman or Docker"} engine with its socket at {podman} or {docker}')


class Smoke:
    def __init__(self, engine, engine_socket, engine_gid):
        self.engine = engine
        self.engine_socket = engine_socket
        self.engine_gid = engine_gid
        # Subprocesses see only this, so nothing the caller exported can change what compose or the engine does.
        self.env = {
            'PATH': os.environ.get('PATH', '/usr/bin:/bin'),
            'HOME': os.environ.get('HOME', '/'),
            'XDG_RUNTIME_DIR': RUNTIME_DIR,
            'DOCKER_HOST': f'unix://{engine_socket}',
        }
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
        self.ready = set()
        self.engine_locked = False
        self.volumes_before = set()
        self.images_before = set()
        self.created = {kind: set() for kind in ('container', 'volume', 'network', 'tag')}
        self.failures = []
        self.timings = {}

    def run(self, argv, log=None, timeout=1800, check=True, **kwargs):
        if log:
            with (self.logs / log).open('w') as out:
                result = subprocess.run(argv, stdout=out, stderr=subprocess.STDOUT, timeout=timeout, env=self.env,
                                        **kwargs)
        else:
            result = subprocess.run(argv, capture_output=True, text=True, timeout=timeout, env=self.env, **kwargs)
        if check and result.returncode != 0:
            detail = f'see {self.logs / log}' if log else (result.stderr or result.stdout).strip()
            raise Failed(f'{" ".join(map(str, argv[:4]))} exited {result.returncode}: {detail}')
        return result

    def ids(self, *argv):
        return {line.removeprefix('sha256:') for line in self.run([self.engine, *argv], timeout=60).stdout.split()}

    def attempt(self, what, work):
        """Runs `work`, recording rather than raising its failure, so one failed step doesn't skip the rest."""
        try:
            return work()
        except Exception as error:
            self.failures.append(f'{what}: {error}')
            print(f'{what} failed: {error}', file=sys.stderr)
            return None

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

    def lock_engine(self):
        """Fences other smoke runs on this engine, whatever user or runtime directory they run under."""
        created = self.run([self.engine, 'network', 'create', ENGINE_LOCK], check=False, timeout=60)
        if created.returncode != 0:
            raise Failed(f'another managed smoke is using this engine ({created.stderr.strip()}); if none is running, '
                         f'a crashed one left its lock behind: remove it with `{self.engine} network rm {ENGINE_LOCK}`')
        self.engine_locked = True

    def unlock_engine(self):
        if self.engine_locked:
            self.attempt(f'removing the {ENGINE_LOCK} network',
                         lambda: self.run([self.engine, 'network', 'rm', ENGINE_LOCK], timeout=60))

    def preflight(self):
        """Refuses to run next to another install or an earlier run's leftovers, which this run must not adopt."""
        self.run([self.engine, 'compose', 'version'])
        if self.run([self.engine, 'network', 'inspect', NETWORK], check=False).returncode == 0:
            raise Failed(f'a `{NETWORK}` network already exists, so another chunk install may be running')
        project = (self.ids('ps', '-a', '-q', '--no-trunc', '--filter', f'label={PROJECT_LABEL}')
                   | self.ids('volume', 'ls', '-q', '--filter', f'label={PROJECT_LABEL}'))
        if project:
            raise Failed(f'the {PROJECT} compose project already has containers or volumes: {sorted(project)}')
        tags = self.run([self.engine, 'images', '--format', '{{.Repository}}:{{.Tag}}']).stdout.split()
        if leftover := [tag for tag in tags if f':{TAG}' in tag]:
            raise Failed(f'images tagged by an earlier run remain: {leftover}')
        self.volumes_before = self.ids('volume', 'ls', '-q')
        self.images_before = self.ids('images', '-a', '-q', '--no-trunc')

    def record(self, label):
        """Records the containers and volumes labelled `label`, and the volumes those containers mount."""
        containers = self.attempt(f'listing containers labelled {label}', lambda: self.ids(
            'ps', '-a', '-q', '--no-trunc', '--filter', f'label={label}')) or set()
        self.created['container'] |= containers
        volumes = self.attempt(f'listing volumes labelled {label}', lambda: self.ids(
            'volume', 'ls', '-q', '--filter', f'label={label}')) or set()
        for container in containers:
            volumes |= self.attempt(f'inspecting container {container}', lambda: self.mounts(container)) or set()
        self.created['volume'] |= volumes - self.volumes_before

    def mounts(self, container):
        found = self.run([self.engine, 'container', 'inspect', container], check=False, timeout=60)
        if found.returncode != 0:
            return set()
        [details] = json.loads(found.stdout)
        return {mount['Name'] for mount in details.get('Mounts') or [] if mount.get('Type') == 'volume'}

    def record_compose(self):
        self.record(PROJECT_LABEL)
        self.attempt(f'inspecting the {NETWORK} network', self.record_network)

    def record_network(self):
        found = self.run([self.engine, 'network', 'inspect', NETWORK], check=False, timeout=60)
        if found.returncode == 0:
            [network] = json.loads(found.stdout)
            if (network.get('Labels') or network.get('labels') or {}).get('com.docker.compose.project') == PROJECT:
                self.created['network'].add(network.get('Id') or network['id'])

    def record_machines(self):
        self.record(f'chunk.environment={self.environment}')

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
            # Preflight saw no tag of this run's, so the tag is this run's once it exists.
            self.created['tag'].add(image)
            self.run([self.engine, 'build', '-f', dockerfile, '-t', image, '--build-arg', f'VERSION={version}',
                      '--build-arg', f'REVISION={revision}', *extra, '.'],
                     f'image-{image.split(":")[0]}.log', cwd=ROOT)

    def start(self):
        self.run([str(BUNDLE / 'init.sh'), str(self.env_file)])
        settings = dict(line.split('=', 1) for line in self.env_file.read_text().splitlines() if '=' in line)
        settings.update({
            'CHUNK_ENGINE_SOCKET': str(self.engine_socket),
            'CHUNK_ENGINE_GID': str(self.engine_gid),
            'CHUNK_PLAYER_BIND': '127.0.0.1',
            'CHUNK_PLAYER_PORT': str(self.player_port),
            'CHUNK_MANAGEMENT_PUBLISH': f'127.0.0.1:{self.management_port}',
            'CHUNK_PUBLIC_URL': self.url,
            'CHUNK_EDGE_DOMAIN': 'localhost',
            'CHUNK_ENVIRONMENT_IMAGE': f'chunk-environment:{TAG}',
            'CHUNK_JVM_IMAGE': f'chunk-jvm:{TAG}-{{java}}',
            'CHUNK_EDGE_IMAGE': f'chunk-edge:{TAG}',
            'CHUNK_MANAGEMENT_IMAGE': f'chunk-management:{TAG}',
        })
        self.env_file.write_text(''.join(f'{name}={value}\n' for name, value in settings.items()))
        self.override.write_text(OVERRIDE)
        self.token = settings['CHUNK_OPERATOR_TOKEN']
        step(f'compose up on {self.engine_socket}: players on 127.0.0.1:{self.player_port}, management at {self.url}')
        self.up = True
        try:
            self.run([*self.compose, 'up', '-d', '--wait', '--wait-timeout', '180'], 'compose-up.log', timeout=300)
        finally:
            self.record_compose()

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
        self.record_machines()
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
                                   '--host', self.hostname], stdout=out, stderr=subprocess.STDOUT, timeout=600,
                                  env=self.env).returncode
        self.record_machines()
        self.dump_logs()
        if code:
            raise Failed(f'the bot scenario failed; see {self.logs / "bot.log"}')
        requests = self.jvm_requests()
        if {app for _, app, _ in requests} != {'lobby', 'arena'} or any(int(s) != READY for *_, s in requests):
            raise Failed(f'expected READY JVM capacity for lobby and arena, got {requests}')
        self.ready = {request for request, _, _ in requests}
        jvms = self.machines('jvm')
        wanted = {self.jvm_image, f'localhost/{self.jvm_image}', f'docker.io/library/{self.jvm_image}'}
        if len(jvms) != len(requests) or any(image not in wanted or state != 'running' for _, image, state in jvms):
            raise Failed(f'expected {len(requests)} running {self.jvm_image} machines, got {jvms}')
        step(f'PASS: {len(jvms)} JVM machines run {self.jvm_image}; their capacity is READY')

    def release(self):
        step('waiting for idle JVMs to be retired and their capacity RELEASED')
        if not self.ready:
            raise Failed('no READY JVM capacity was observed')

        def released():
            states = {request: int(state) for request, _, state in self.jvm_requests()}
            return all(states.get(request) == RELEASED for request in self.ready)
        wait_for(released, f'JVM capacity {sorted(self.ready)} to be released', 240, interval=5)
        wait_for(lambda: not self.machines('jvm'), 'released JVM machines to be removed', 60)
        step(f'PASS: all {len(self.ready)} READY JVM requests RELEASED and their machines removed')

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

    def present(self, kind):
        """Which of this run's recorded items of `kind` still exist; None if listing them failed."""
        if kind == 'tag':
            return {tag for tag in self.created[kind] if self.attempt(f'inspecting image {tag}',
                                                                      lambda: self.image_id(tag))}
        listed = self.attempt(f'listing {kind}s', lambda: self.ids(*LISTINGS[kind]))
        return None if listed is None else self.created[kind] & listed

    def remove_leftovers(self):
        """Removes recorded containers, volumes and networks that the environment delete and compose down missed."""
        leftovers = {}
        for kind, removal in REMOVALS.items():
            present = self.present(kind)
            # Without a listing, try every recorded item; removing one that is gone fails harmlessly.
            for item in sorted(self.created[kind] if present is None else present):
                self.attempt(f'removing {kind} {item}', lambda: self.run(
                    [self.engine, *removal, item], check=False, timeout=120))
            if present:
                leftovers[kind] = sorted(present)
        if leftovers:
            self.failures.append(f'left behind by the environment delete and compose down, now removed: {leftovers}')

    def image_id(self, tag):
        """The ID of the image `tag` names, or None if there is none; raises if the engine can't tell."""
        found = self.run([self.engine, 'image', 'inspect', '--format', '{{.Id}}', tag], check=False, timeout=60)
        if found.returncode == 0:
            return found.stdout.strip().removeprefix('sha256:')
        if any(missing in found.stderr.lower() for missing in ('image not known', 'no such image')):
            return None
        raise Failed(f'image inspect {tag} exited {found.returncode}: {found.stderr.strip()}')

    def remove_tag(self, tag):
        """Removes a smoke tag, leaving the image and its layers as build cache."""
        image = self.image_id(tag)
        if image is None:
            return
        if self.engine == 'podman':
            self.run([self.engine, 'untag', image, tag], timeout=120)
        elif image in self.images_before:
            # Docker can't untag without deleting an image's last tag, and this image isn't the run's to delete.
            self.created['tag'].discard(tag)
            print(f'note: left {tag} on {image[:12]}, which predates this run; `docker image rm {tag}` removes it')
        else:
            self.run([self.engine, 'rmi', '--no-prune', tag], timeout=120)

    def remove_files(self, passed):
        # The secrets go with the install; a failed run keeps its release for a rerun by hand.
        for path in (self.env_file, self.override):
            self.attempt(f'removing {path}', lambda: path.unlink(missing_ok=True))
        if passed:
            shutil.rmtree(self.work / 'release', ignore_errors=True)

    def check_inventory(self):
        left = {kind: sorted(items) for kind in (*REMOVALS, 'tag') if (items := self.present(kind))}
        if left:
            self.failures.append(f'still present: {left}')

    def cleanup(self, passed):
        """Removes what this run recorded creating, each step best effort, and returns every failure of the run."""
        if self.up and self.environment:
            self.attempt('deleting the environment', lambda: self.delete(timeout=90))
        if self.up:
            self.attempt('compose down', lambda: self.run([*self.compose, 'down', '-v', '--timeout', '20'],
                                                          'compose-down.log', timeout=300))
            self.record_compose()
        if self.environment:
            self.record_machines()
        self.remove_leftovers()
        for tag in sorted(self.created['tag']):
            self.attempt(f'removing tag {tag}', lambda: self.remove_tag(tag))
        self.remove_files(passed)
        self.check_inventory()
        self.unlock_engine()
        return self.failures


def interrupted(signum, frame):
    raise KeyboardInterrupt


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--engine', choices=['podman', 'docker'],
                        help='the engine to use; by default rootless Podman if its socket exists, else Docker')
    args = parser.parse_args()
    signal.signal(signal.SIGTERM, interrupted)

    lock = LOCK.open('a')
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        print(f'STOPPED: another managed smoke holds {LOCK}; nothing was changed', file=sys.stderr)
        return 2
    smoke = None
    try:
        smoke = Smoke(*choose_engine(args.engine))
        smoke.lock_engine()
        smoke.preflight()
    except (Exception, KeyboardInterrupt) as error:
        reason = error if isinstance(error, Failed) else f'{type(error).__name__}: {error}'
        print(f'STOPPED: {reason}; nothing was changed', file=sys.stderr)
        if smoke:
            smoke.unlock_engine()
            shutil.rmtree(smoke.work)
        return 2
    step(f'engine {smoke.engine} on {smoke.engine_socket}; logs: {smoke.logs}')
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
        failures = smoke.timed('cleanup', lambda: smoke.cleanup(passed))
    if not failures:
        step('PASS: nothing this run created remains')
    print('timings: ' + ', '.join(f'{name} {seconds:.0f}s' for name, seconds in smoke.timings.items()))
    print(f'{"PASS" if passed and not failures else "FAIL"}; logs in {smoke.logs}')
    return 0 if passed and not failures else 1


if __name__ == '__main__':
    sys.exit(main())
