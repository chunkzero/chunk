#!/usr/bin/env python3
"""Runs chunk-bots from several hosts at once and collects their summaries.

Each host gets an even, disjoint slice of the bots (by `--first`), a copy of the binary under /tmp, and the same
chunk-bots arguments, so `--login-rate` applies per host. `local` runs on this machine instead of over SSH. Summaries and
logs land in `--output`, with `summary.json` combining their counts; latency percentiles stay per host. Ctrl-C, or a
failure while starting, stops this invocation's bots on every host, which still print their summaries; other runs on
the same hosts are left alone. Remote bots also end on their own: they run under `timeout` and an SSH session with a
terminal, so a lost connection hangs them up. Remote hosts therefore need a finite `--hold`.

    scripts/load/run.py --hosts bots-1,bots-2,bots-3 --bots 5000 -- --address play.example.com:25565 --hold 600
"""
import argparse
import json
import math
from pathlib import Path
import shlex
import subprocess
import sys
import time
import uuid

ROOT = Path(__file__).resolve().parents[2]
# Seconds the bots get to print their summaries after being told to stop, and to finish after their expected end.
STOP_WAIT = 30
UPLOAD_TIMEOUT = 120
# Extra seconds, past a run's watchdog, before its SSH process is abandoned.
MARGIN = 120
# A watchdog's exit status, and `--kill-after`'s; either means the bots were cut off.
WATCHDOG_EXITS = (124, 137)
GRACE = 60
# Bound for each cleanup command, so a stalled host can't hold up the others.
CLEANUP_TIMEOUT = 30
SSH_OPTIONS = ['-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', '-o', 'ServerAliveInterval=5', '-o',
               'ServerAliveCountMax=3']
COUNTS = ('started', 'logged_in', 'spawned', 'playing', 'failed', 'disconnects', 'reconfigurations', 'pings', 'pongs',
          'ping_timeouts', 'commands', 'bytes_in', 'cpu_seconds')


def command(host, script):
    """Runs a POSIX shell script on `host`; remotely under a terminal, which hangs the script up if SSH drops."""
    if host == 'local':
        return ['sh', '-c', script]
    return ['ssh', '-tt', *SSH_OPTIONS, host, shlex.join(['sh', '-c', script])]


def cleanup(host, script, **kwargs):
    """Runs a bounded command on `host`, returning whether it succeeded."""
    try:
        return subprocess.run(command(host, script), stdin=subprocess.DEVNULL, timeout=CLEANUP_TIMEOUT,
                              **kwargs).returncode == 0
    except subprocess.TimeoutExpired:
        return False


def stop(runs):
    """Interrupts this invocation's bots by the PID each recorded at launch, killing SSH sessions that don't finish."""
    for run in runs:
        cleanup(run['host'], f'kill -INT "$(cat {run["base"]}.pid)"', stderr=subprocess.DEVNULL)
    for run in runs:
        if run['process'] is not None:
            try:
                run['process'].wait(timeout=STOP_WAIT)
            except subprocess.TimeoutExpired:
                run['process'].kill()


def collect(run, output):
    """Copies the run's results to `output`, then removes its files from the host once its bots are gone."""
    for extension in ('json', 'log'):
        with open(output / f'{run["name"]}.{extension}', 'w') as file:
            cleanup(run['host'], f'cat {run["base"]}.{extension}', stdout=file, stderr=subprocess.DEVNULL)
    if cleanup(run['host'], f'! kill -0 "$(cat {run["base"]}.pid)" 2>/dev/null'):
        files = ' '.join(f'{run["base"]}{suffix}' for suffix in ('', '.pid', '.json', '.log'))
        cleanup(run['host'], f'rm -f {files}')
    else:
        print(f'{run["host"]}: bots may still be running; their PID is in {run["base"]}.pid', file=sys.stderr)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--hosts', required=True, help='comma-separated SSH hosts, or local')
    parser.add_argument('--bots', type=int, required=True, help='bots across all hosts')
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/release/chunk-bots')
    parser.add_argument('--output', type=Path, default=ROOT / f'target/load/{time.strftime("%Y%m%d-%H%M%S")}')
    parser.add_argument('args', nargs=argparse.REMAINDER, help='chunk-bots arguments, after --')
    options = parser.parse_args()
    hosts = options.hosts.split(',')
    arguments = [argument for argument in options.args if argument != '--']
    timing = argparse.ArgumentParser(add_help=False, allow_abbrev=False)
    timing.add_argument('--hold', type=int, default=60)
    timing.add_argument('--login-rate', type=float, default=20)
    timing.add_argument('--max-pending', type=int, default=32)
    timing = timing.parse_known_args(arguments)[0]
    if timing.hold == 0 and any(host != 'local' for host in hosts):
        parser.error('remote hosts need a finite --hold, so their bots end on their own')
    options.output.mkdir(parents=True, exist_ok=True)

    marker = uuid.uuid4().hex[:8]
    runs = []
    try:
        for index, host in enumerate(hosts):
            first = options.bots * index // len(hosts)
            count = options.bots * (index + 1) // len(hosts) - first
            base = f'/tmp/chunk-bots-{marker}-{index}'
            run = {'host': host, 'name': f'{index}-{host}', 'base': base, 'process': None, 'expires': None,
                   'incomplete': False}
            runs.append(run)
            binary = str(options.binary.resolve())
            if host != 'local':
                upload = ['scp', '-q', *SSH_OPTIONS, binary, f'{host}:{base}']
                subprocess.run(upload, check=True, timeout=UPLOAD_TIMEOUT)
                binary = base
            bots = shlex.join([binary, '--json', '--bots', str(count), '--first', str(first), *arguments])
            if timing.hold > 0:
                # Logins that take about ten seconds hold the ramp to `--max-pending` per ten seconds.
                ramp = count / min(timing.login_rate, timing.max_pending / 10)
                limit = math.ceil(timing.hold + ramp + GRACE)
                bots = f'timeout --signal=INT --kill-after=10 {limit} {bots}'
                run['expires'] = time.monotonic() + limit + MARGIN
            script = f'ulimit -n "$(ulimit -Hn)"; echo $$ > {base}.pid; exec {bots} > {base}.json 2> {base}.log'
            run['process'] = subprocess.Popen(command(host, script), stdin=subprocess.DEVNULL, start_new_session=True)
            print(f'{host}: bots {first}..{first + count - 1}', flush=True)
        for run in runs:
            try:
                remaining = None if run['expires'] is None else max(0, run['expires'] - time.monotonic())
                run['process'].wait(timeout=remaining)
            except subprocess.TimeoutExpired:
                print(f'{run["host"]}: still running past its watchdog; stopping it', file=sys.stderr)
                run['incomplete'] = True
                stop([run])
    except BaseException as error:
        stop(runs)
        if not isinstance(error, KeyboardInterrupt):
            raise
    finally:
        for run in runs:
            collect(run, options.output)

    total = dict.fromkeys(COUNTS, 0)
    hosts_summary = {}
    for run in runs:
        host, name = run['host'], run['name']
        if run['process'] and run['process'].returncode in WATCHDOG_EXITS:
            run['incomplete'] = True
        try:
            summary = json.loads((options.output / f'{name}.json').read_text())['summary']
        except (ValueError, KeyError):
            code = run['process'].returncode if run['process'] else 'never started'
            print(f'{host}: no summary (exit {code}); see {options.output / (name + ".log")}')
            continue
        hosts_summary[name] = summary
        if run['incomplete']:
            print(f'{host}: incomplete, the watchdog ended it before its hold finished')
        for key in COUNTS:
            total[key] += summary[key]
        login, rtt = summary['login_to_play_ms'], summary['ping_rtt_ms']
        print(f'{host}: playing {summary["playing"]}/{summary["started"]}, failed {summary["failed"]}, '
              f'disconnects {summary["disconnects"]}, login p50/p99 {login["p50"]}/{login["p99"]} ms, '
              f'rtt p50/p99 {rtt["p50"]}/{rtt["p99"]} ms, cpu {summary["cpu_cores"]} cores')
    (options.output / 'summary.json').write_text(json.dumps({'total': total, 'hosts': hosts_summary}, indent=2))
    print(f'total: {json.dumps(total)}\nresults: {options.output}')
    return 0 if len(hosts_summary) == len(runs) and not any(run['incomplete'] for run in runs) else 1


if __name__ == '__main__':
    sys.exit(main())
