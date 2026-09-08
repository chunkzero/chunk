#!/usr/bin/env python3
"""Quick functional pass over every Bun cell with small sample counts."""
import json, os, subprocess, sys
HERE = os.path.dirname(os.path.abspath(__file__))
cells = [(e, w, 0) for e in ["inline", "persistent", "persistent-vm", "fresh-vm", "fresh-esm", "fresh-realm"] for w in ["empty", "reads", "query", "sync"]]
cells += [(e, "query", 128) for e in ["fresh-vm", "fresh-esm", "fresh-realm", "persistent"]]
cells += [(p, "empty", 128) for p in ["context", "realm", "worker", "cold", "cached", "terminate", "vmtimeout"]]
env = {**os.environ, "BENCH_WARMUP": os.environ.get("BENCH_WARMUP", "20"), "BENCH_CALLS": os.environ.get("BENCH_CALLS", "200")}
failed = False
for engine, workload, kib in cells:
    r = subprocess.run(["bun", os.path.join(HERE, "main.ts"), engine, workload, str(kib)], capture_output=True, text=True, env=env, timeout=300)
    line = next((l for l in r.stdout.splitlines() if l.startswith("{")), None)
    if r.returncode or not line:
        failed = True
        print(f"{engine} {workload} {kib} FAILED\n{r.stdout[-800:]}{r.stderr[-800:]}")
        continue
    d = json.loads(line)
    sync = d["sync"] and {k: v for k, v in d["sync"].items() if k != "leaderboard"}
    print(f"{engine:14} {workload:6} {d['bundle_bytes']:>7}B wall med/p99 {d['wall_us']['median']:8.1f}/{d['wall_us']['p99']:8.1f} us  cpu/call {d['process_cpu_us_per_call']:8.1f} us  {sync or ''}")
sys.exit(1 if failed else 0)
