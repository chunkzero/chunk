#!/usr/bin/env python3
"""Render comparison tables from both harnesses' result files."""
import json, pathlib, sys
HERE = pathlib.Path(__file__).resolve().parent
rows = []
for path in sorted((HERE / "engine/results").glob("*.jsonl")) + sorted((HERE / "bun/results").glob("*.jsonl")):
    if not path.name.startswith(("rust-", "bun-")):
        continue
    for line in path.read_text().splitlines():
        d = json.loads(line)
        d["_file"] = path.stem
        d["_engine"] = d["engine"] if d["engine"].startswith("bun-") else f"rust-{d['engine']}"
        rows.append(d)
def rss(d):
    return round(int(d["peak_rss"].split()[1]) / 1024, 1) if d["peak_rss"] else None
def table(title, cells, columns):
    print(f"\n### {title}\n")
    print("| " + " | ".join(c[0] for c in columns) + " |")
    print("|" + "|".join(" ---: " if c[2] else " --- " for c in columns) + "|")
    for d in cells:
        print("| " + " | ".join(str(c[1](d)) for c in columns) + " |")
ORDER = ["rust-deno", "rust-fresh", "rust-persistent", "bun-inline", "bun-persistent", "bun-persistent-vm", "bun-fresh-vm", "bun-fresh-esm", "bun-fresh-realm"]
def key(d):
    return (d["bundle_bytes"], ["empty", "reads", "query", "sync"].index(d["export"]) if d["export"] in ["empty", "reads", "query", "sync"] else 9, ORDER.index(d["_engine"]) if d["_engine"] in ORDER else 99)
ms = lambda v: f"{v / 1000:.3f}"
unrestricted = sorted([d for d in rows if d["burst_per_80ms"] == 0 and d["_file"].endswith(("unrestricted", "deno-tiny", "deno-large"))], key=key)
table("Unrestricted local comparison", unrestricted, [
    ("Bundle bytes", lambda d: d["bundle_bytes"], True), ("Workload", lambda d: d["export"], False), ("Engine", lambda d: d["_engine"], False),
    ("Wall median ms", lambda d: ms(d["wall_us"]["median"]), True), ("Wall p99 ms", lambda d: ms(d["wall_us"]["p99"]), True),
    ("Mean process CPU ms/call", lambda d: ms(d["process_cpu_us_per_call"]), True), ("Peak RSS MiB", rss, True)])
prims = [d for d in rows if d["_file"].endswith(("primitives", "nocache"))]
table("Component measurements (128 KiB bundle)", prims, [
    ("Operation", lambda d: f"{d['_engine']}" + (" (no cache)" if d["cache_stage"] == "none" else ""), False),
    ("Median ms", lambda d: ms(d["termination_us"]["median"] if d["termination_us"] else d["module_us"]["median"] if d["engine"] == "cold" and d["_engine"].startswith("bun") else d["wall_us"]["median"]), True),
    ("p99 ms", lambda d: ms(d["termination_us"]["p99"] if d["termination_us"] else d["module_us"]["p99"] if d["engine"] == "cold" and d["_engine"].startswith("bun") else d["wall_us"]["p99"]), True),
    ("Mean process CPU ms", lambda d: ms(d["process_cpu_us_per_call"]), True), ("Peak RSS MiB", rss, True)])
quota = sorted([d for d in rows if d["burst_per_80ms"] > 0], key=key)
def throttled(d):
    def parse(s):
        return {k: int(v) for k, v in (l.split() for l in s.strip().splitlines() if l)}
    a, b = parse(d["cpu_stat_before"]), parse(d["cpu_stat_after"])
    return f"{b.get('nr_throttled', 0) - a.get('nr_throttled', 0)}/{b.get('nr_periods', 0) - a.get('nr_periods', 0)}"
table("Local 10 ms / 80 ms CPU quota, burst arrivals", quota, [
    ("Bundle bytes", lambda d: d["bundle_bytes"], True), ("Workload", lambda d: d["export"], False), ("Engine", lambda d: d["_engine"], False),
    ("Arrivals per 80 ms", lambda d: d["burst_per_80ms"], True), ("Offered/s", lambda d: d["burst_per_80ms"] * 12.5, True),
    ("Completed/s", lambda d: f"{d['calls_per_second']:.1f}", True),
    ("Wall median ms", lambda d: ms(d["wall_us"]["median"]), True), ("Wall p99 ms", lambda d: ms(d["wall_us"]["p99"]), True),
    ("Mean process CPU ms/call", lambda d: ms(d["process_cpu_us_per_call"]), True),
    ("Response p99 ms", lambda d: ms(d["response_us"]["p99"]), True), ("Max response ms", lambda d: ms(d["response_us"]["max"]), True),
    ("Throttled periods", throttled, False)])
