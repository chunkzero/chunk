#!/usr/bin/env python3
"""Quick functional pass over every Rust cell with small sample counts."""
import json, os, subprocess, sys
HERE = os.path.dirname(os.path.abspath(__file__))
binary = os.path.join(HERE, "target/release/chunk-js-engine-bench")
cells = [(e, w, 0) for e in ["deno", "fresh", "persistent", "tuned", "actor"] for w in ["empty", "reads", "query", "sync"]]
cells += [(e, "query", 128) for e in ["fresh", "persistent", "tuned", "actor"]]
cells += [(p, "empty", 128) for p in ["isolate", "context", "cold", "cached", "terminate"]]
env = {**os.environ, "BENCH_WARMUP": os.environ.get("BENCH_WARMUP", "20"), "BENCH_CALLS": os.environ.get("BENCH_CALLS", "200")}
failed = False
for engine, workload, kib in cells:
    r = subprocess.run([binary, engine, workload, str(kib)], capture_output=True, text=True, env=env, timeout=600)
    line = next((l for l in r.stdout.splitlines() if l.startswith("{")), None)
    if r.returncode or not line:
        failed = True
        print(f"{engine} {workload} {kib} FAILED\n{r.stdout[-800:]}{r.stderr[-800:]}")
        continue
    d = json.loads(line)
    sync = d["sync"] and {k: v for k, v in d["sync"].items() if k != "leaderboard"}
    print(f"{engine:14} {workload:6} {d['bundle_bytes']:>7}B wall med/p99 {d['wall_us']['median']:8.1f}/{d['wall_us']['p99']:8.1f} us  cpu/call {d['process_cpu_us_per_call']:8.1f} us  {sync or ''}")
sys.exit(1 if failed else 0)
