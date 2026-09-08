#!/usr/bin/env python3
"""Run Bun cells sequentially under the same cgroup limits as the Rust harness."""
import argparse
import json
import pathlib
import subprocess

HERE = pathlib.Path(__file__).resolve().parent
parser = argparse.ArgumentParser()
parser.add_argument("profile", choices=["unrestricted", "quota"])
parser.add_argument("--engines", nargs="+")
parser.add_argument("--workloads", nargs="+")
parser.add_argument("--sizes", nargs="+", type=int)
parser.add_argument("--bursts", nargs="+", type=int, default=[10, 25])
parser.add_argument("--cache", choices=["instantiate", "none"], default="instantiate")
parser.add_argument("--init", choices=["eager", "declarations"], default="eager")
parser.add_argument("--name")
args = parser.parse_args()
args.engines = args.engines or (["inline", "persistent", "persistent-vm", "fresh-vm", "fresh-esm", "fresh-realm"]
                                if args.profile == "unrestricted" else ["persistent", "fresh-vm"])
args.workloads = args.workloads or (["empty", "reads", "query", "sync"] if args.profile == "unrestricted" else ["query", "sync"])
args.sizes = args.sizes if args.sizes is not None else ([0, 128] if args.profile == "unrestricted" else [128])
output = HERE / "results" / f"{args.name or args.profile}.jsonl"
output.parent.mkdir(exist_ok=True)
if args.profile == "unrestricted":
    cells = [(engine, workload, size, 0) for size in args.sizes for workload in args.workloads for engine in args.engines]
else:
    cells = [(engine, workload, size, burst) for size in args.sizes for workload in args.workloads
             for burst in args.bursts for engine in args.engines]
bun = subprocess.check_output(["which", "bun"], text=True).strip()
with output.open("w") as file:
    for engine, workload, size, burst in cells:
        environment = [f"BENCH_CACHE={args.cache}", f"BENCH_INIT={args.init}"]
        command = ["systemd-run", "--user", "--wait", "--pipe", "--collect", "-p", "CPUAffinity=0 1", "-p", "MemoryMax=512M"]
        if args.profile == "quota":
            command += ["-p", "CPUQuota=12.5%", "-p", "CPUQuotaPeriodSec=80ms"]
        command += ["/usr/bin/env", *environment, bun, str(HERE / "main.ts"), engine, workload, str(size), str(burst)]
        print(f"Running bun {engine} {workload} {size} KiB burst={burst}", flush=True)
        result = subprocess.run(command, capture_output=True, text=True, timeout=1800)
        if result.returncode:
            raise RuntimeError(result.stdout + result.stderr)
        data = next(json.loads(line) for line in result.stdout.splitlines() if line.startswith("{"))
        if args.profile == "quota":
            assert data["cpu_max"] == "10000 80000", data["cpu_max"]
        file.write(json.dumps(data) + "\n")
        file.flush()
        print(f"  wall median/p99 {data['wall_us']['median']:.1f}/{data['wall_us']['p99']:.1f} us; "
              f"CPU/call {data['process_cpu_us_per_call']:.1f} us; "
              f"response p99 {data['response_us']['p99']:.1f} us", flush=True)
print(output)
