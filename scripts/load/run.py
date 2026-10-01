#!/usr/bin/env python3
"""Runs chunk-bots from several hosts at once and collects their summaries.

Each host gets an even, disjoint slice of the bots (by `--first`), a copy of the binary under /tmp, and the same
chunk-bots arguments, so `--login-rate` applies per host. `local` runs on this machine instead of over SSH. Summaries and
logs land in `--output`, with `summary.json` combining their counts; latency percentiles stay per host. Ctrl-C, or a
failure while starting, stops this invocation's bots on every host, which still print their summaries; other runs on
the same hosts are left alone.

    scripts/load/run.py --hosts bots-1,bots-2,bots-3 --bots 5000 -- --address play.example.com:25565 --hold 600
"""
import argparse
import json
from pathlib import Path
import shlex
import subprocess
import sys
import time
import uuid

ROOT = Path(__file__).resolve().parents[2]
# Seconds the bots get to print their summaries after being told to stop.
STOP_WAIT = 30
COUNTS = ('started', 'logged_in', 'spawned', 'playing', 'failed', 'disconnects', 'reconfigurations', 'pings', 'pongs',
          'ping_timeouts', 'commands', 'bytes_in', 'cpu_seconds')


def command(host, script):
    """Runs a POSIX shell script on `host`."""
    if host == 'local':
        return ['sh', '-c', script]
    return ['ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', host, shlex.join(['sh', '-c', script])]


def stop(runs):
    """Interrupts the bots of this invocation that are still running, by the PID each recorded at launch."""
    live = [run for run in runs if run['process'].poll() is None]
    for run in live:
        subprocess.run(command(run['host'], f'kill -INT "$(cat {run["pidfile"]})" 2>/dev/null'))
    for run in live:
        try:
            run['process'].wait(timeout=STOP_WAIT)
        except subprocess.TimeoutExpired:
            run['process'].kill()


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
    options.output.mkdir(parents=True, exist_ok=True)

    marker = uuid.uuid4().hex[:8]
    touched = []
    runs = []
    try:
        for index, host in enumerate(hosts):
            first = options.bots * index // len(hosts)
            count = options.bots * (index + 1) // len(hosts) - first
            binary = str(options.binary.resolve())
            pidfile = f'/tmp/chunk-bots-{marker}-{index}.pid'
            touched.append((host, pidfile))
            if host != 'local':
                remote = f'/tmp/chunk-bots-{marker}-{index}'
                touched.append((host, remote))
                subprocess.run(['scp', '-q', binary, f'{host}:{remote}'], check=True)
                binary = remote
            bots = shlex.join([binary, '--json', '--bots', str(count), '--first', str(first), *arguments])
            script = f'ulimit -n "$(ulimit -Hn)"; echo $$ > {pidfile}; exec {bots}'
            name = f'{index}-{host}'
            with (open(options.output / f'{name}.json', 'w') as stdout,
                  open(options.output / f'{name}.log', 'w') as stderr):
                process = subprocess.Popen(command(host, script), stdout=stdout, stderr=stderr, start_new_session=True)
            runs.append({'host': host, 'name': name, 'process': process, 'pidfile': pidfile})
            print(f'{host}: bots {first}..{first + count - 1}', flush=True)
        for run in runs:
            run['process'].wait()
    except KeyboardInterrupt:
        stop(runs)
    except BaseException:
        stop(runs)
        raise
    finally:
        for host, path in touched:
            subprocess.run(command(host, f'rm -f {path}'))

    total = dict.fromkeys(COUNTS, 0)
    hosts_summary = {}
    for run in runs:
        host, name = run['host'], run['name']
        try:
            summary = json.loads((options.output / f'{name}.json').read_text())['summary']
        except (ValueError, KeyError):
            print(f'{host}: no summary (exit {run["process"].returncode}); see {options.output / (name + ".log")}')
            continue
        hosts_summary[name] = summary
        for key in COUNTS:
            total[key] += summary[key]
        login, rtt = summary['login_to_play_ms'], summary['ping_rtt_ms']
        print(f'{host}: playing {summary["playing"]}/{summary["started"]}, failed {summary["failed"]}, '
              f'disconnects {summary["disconnects"]}, login p50/p99 {login["p50"]}/{login["p99"]} ms, '
              f'rtt p50/p99 {rtt["p50"]}/{rtt["p99"]} ms, cpu {summary["cpu_cores"]} cores')
    (options.output / 'summary.json').write_text(json.dumps({'total': total, 'hosts': hosts_summary}, indent=2))
    print(f'total: {json.dumps(total)}\nresults: {options.output}')
    return 0 if len(hosts_summary) == len(runs) else 1


if __name__ == '__main__':
    sys.exit(main())
