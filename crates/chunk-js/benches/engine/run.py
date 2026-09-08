#!/usr/bin/env python3
"""Run cells sequentially so benchmark processes never compete with each other."""
import argparse
import json
import pathlib
import shlex
import subprocess

HERE = pathlib.Path(__file__).resolve().parent
parser = argparse.ArgumentParser()
parser.add_argument("profile", choices=["unrestricted", "quota", "fly-quota"])
parser.add_argument("--engines", nargs="+")
parser.add_argument("--workloads", nargs="+")
parser.add_argument("--sizes", nargs="+", type=int)
parser.add_argument("--bursts", nargs="+", type=int, default=[10, 25])
parser.add_argument("--cache", choices=["instantiate", "evaluate"], default="instantiate")
parser.add_argument("--init", choices=["eager", "declarations"], default="eager")
parser.add_argument("--name")
parser.add_argument("--app")
parser.add_argument("--machine")
args = parser.parse_args()
args.engines = args.engines or (["deno", "fresh", "persistent"] if args.profile == "unrestricted" else ["fresh", "persistent"])
args.workloads = args.workloads or (["empty", "reads", "query"] if args.profile == "unrestricted" else ["query"])
args.sizes = args.sizes if args.sizes is not None else ([0, 128] if args.profile == "unrestricted" else [128])
binary = HERE / "target/release/chunk-js-engine-bench"
output = HERE / "results" / f"{args.name or args.profile}.jsonl"
output.parent.mkdir(exist_ok=True)
if args.profile == "unrestricted":
    cells = [(engine, workload, size, 0) for size in args.sizes
             for workload in args.workloads for engine in args.engines]
else:
    cells = [(engine, workload, size, burst) for size in args.sizes
             for workload in args.workloads for burst in args.bursts for engine in args.engines]
with output.open("w") as file:
    for engine, workload, size, burst in cells:
        environment = [f"BENCH_CACHE={args.cache}", f"BENCH_INIT={args.init}"]
        cell = [engine, workload, str(size), str(burst)]
        if args.profile == "fly-quota":
            assert args.app and args.machine, "Fly cells require --app and --machine"
            remote = shlex.join(["env", *environment, "/root/fly-quota.sh", *cell])
            command = ["fly", "ssh", "console", "--app", args.app, "--machine", args.machine, "-C", remote]
        else:
            command = ["systemd-run", "--user", "--wait", "--pipe", "--collect",
                       "-p", "CPUAffinity=0 1", "-p", "MemoryMax=512M"]
            if args.profile == "quota":
                command += ["-p", "CPUQuota=12.5%", "-p", "CPUQuotaPeriodSec=80ms"]
            command += ["/usr/bin/env", *environment, str(binary), *cell]
        print(f"Running {engine} {workload} {size} KiB burst={burst}", flush=True)
        result = subprocess.run(command, capture_output=True, text=True, timeout=900)
        if result.returncode:
            raise RuntimeError(result.stdout + result.stderr)
        data = next(json.loads(line) for line in result.stdout.splitlines() if line.startswith("{"))
        if args.profile != "unrestricted":
            assert data["cpu_max"] == "10000 80000", data["cpu_max"]
        file.write(json.dumps(data) + "\n")
        file.flush()
        print(f"  wall median/p99 {data['wall_us']['median']:.1f}/{data['wall_us']['p99']:.1f} us; "
              f"CPU/call {data['process_cpu_us_per_call']:.1f} us; "
              f"response p99 {data['response_us']['p99']:.1f} us", flush=True)
print(output)
