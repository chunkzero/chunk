#!/bin/sh
set -eu
# Run inside an expendable Fly benchmark Machine (cgroup v1).
benchmark_group=/sys/fs/cgroup/cpu,cpuacct/chunk-js-benchmark
mkdir -p "$benchmark_group"
printf '80000\n' > "$benchmark_group/cpu.cfs_period_us"
printf '10000\n' > "$benchmark_group/cpu.cfs_quota_us"
printf '%s\n' "$$" > "$benchmark_group/cgroup.procs"
exec /root/chunk-js-engine-bench "$@"
