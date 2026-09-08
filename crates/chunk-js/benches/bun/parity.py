#!/usr/bin/env python3
"""Functional equivalence check: both harnesses must agree on bundle bytes, result
checks, and the sync engine's evaluation/publication counts and final leaderboard."""
import json, os, subprocess, sys
HERE = os.path.dirname(os.path.abspath(__file__))
rust = os.path.join(HERE, "../engine/target/release/chunk-js-engine-bench")
env = {**os.environ, "BENCH_WARMUP": "20", "BENCH_CALLS": "200"}
def cell(command):
    r = subprocess.run(command, capture_output=True, text=True, env=env, timeout=600)
    if r.returncode:
        raise SystemExit(f"{command} failed:\n{r.stdout[-500:]}{r.stderr[-500:]}")
    return json.loads(next(l for l in r.stdout.splitlines() if l.startswith("{")))
ok = True
for workload in ["empty", "reads", "query", "sync"]:
    for kib in [0, 128]:
        results = {"rust-deno": cell([rust, "deno", workload, str(kib)]) if kib == 0 else None,
                   "rust-fresh": cell([rust, "fresh", workload, str(kib)]),
                   "rust-persistent": cell([rust, "persistent", workload, str(kib)]),
                   "bun-inline": cell(["bun", os.path.join(HERE, "main.ts"), "inline", workload, str(kib)]),
                   "bun-persistent": cell(["bun", os.path.join(HERE, "main.ts"), "persistent", workload, str(kib)]),
                   "bun-fresh-vm": cell(["bun", os.path.join(HERE, "main.ts"), "fresh-vm", workload, str(kib)]),
                   "bun-fresh-realm": cell(["bun", os.path.join(HERE, "main.ts"), "fresh-realm", workload, str(kib)])}
        results = {k: v for k, v in results.items() if v}
        def normalize(sync):
            if not sync:
                return None
            board = sync["leaderboard"]
            board = json.loads(board) if isinstance(board, str) else board
            return json.dumps({"evaluations": sync["evaluations"], "published": sync["published"], "revision": sync["revision"], "leaderboard": board}, sort_keys=True)
        keys = {name: (d["bundle_bytes"], d["calls"], normalize(d["sync"])) for name, d in results.items()}
        distinct = set(keys.values())
        status = "ok" if len(distinct) == 1 else "MISMATCH"
        ok &= len(distinct) == 1
        print(f"{workload:6} {kib:>3} KiB {status}: bytes={next(iter(distinct))[0]} " + ("" if len(distinct) == 1 else json.dumps(keys)))
sys.exit(0 if ok else 1)
