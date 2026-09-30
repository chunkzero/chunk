#!/usr/bin/env python3
"""Runs chunk-bots from several hosts at once and collects their summaries.

Each host gets an even, disjoint slice of the bots (by `--first`), copies of the binary under /tmp, and the same
chunk-bots arguments, so `--login-rate` applies per host. `local` runs on this machine instead of over SSH. Summaries and
logs land in `--output`, with `summary.json` combining their counts; latency percentiles stay per host. Ctrl-C stops
every host's bots, which still print their summaries.

    scripts/load/run.py --hosts bots-1,bots-2,bots-3 --bots 5000 -- --address play.example.com:25565 --hold 600
"""
import argparse
import json
from pathlib import Path
import shlex
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
REMOTE = '/tmp/chunk-bots'
COUNTS = ('started', 'logged_in', 'spawned', 'playing', 'failed', 'disconnects', 'reconfigurations', 'pings', 'pongs',
          'commands', 'bytes_in', 'cpu_seconds')


def command(host, remote):
    """Wraps a shell command for `host`, raising the open-file limit for the bots' sockets."""
    shell = f'ulimit -n "$(ulimit -Hn)"; exec {remote}'
    return ['sh', '-c', shell] if host == 'local' else ['ssh', '-o', 'BatchMode=yes', host, shell]


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

    runs = []
    for index, host in enumerate(hosts):
        first = options.bots * index // len(hosts)
        count = options.bots * (index + 1) // len(hosts) - first
        binary = str(options.binary.resolve())
        if host != 'local':
            subprocess.run(['scp', '-q', binary, f'{host}:{REMOTE}'], check=True)
            binary = REMOTE
        bots = shlex.join([binary, '--json', '--bots', str(count), '--first', str(first), *arguments])
        name = f'{index}-{host}'
        stdout = open(options.output / f'{name}.json', 'w')
        stderr = open(options.output / f'{name}.log', 'w')
        runs.append((host, name, subprocess.Popen(command(host, bots), stdout=stdout, stderr=stderr,
                                                    start_new_session=True)))
        print(f'{host}: bots {first}..{first + count - 1}', flush=True)

    try:
        for _, _, process in runs:
            process.wait()
    except KeyboardInterrupt:
        # The bracket keeps the pattern from matching pkill's own shell.
        for host, _, _ in runs:
            subprocess.run(command(host, "pkill -INT -f '[c]hunk-bots --json'"))
        for _, _, process in runs:
            process.wait()

    total = dict.fromkeys(COUNTS, 0)
    hosts_summary = {}
    for host, name, process in runs:
        try:
            summary = json.loads((options.output / f'{name}.json').read_text())['summary']
        except (ValueError, KeyError):
            print(f'{host}: no summary (exit {process.returncode}); see {options.output / (name + ".log")}')
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
