#!/usr/bin/env bash
# Sequential measurement run for both harnesses (temporary experiment driver).
set -euo pipefail
cd "$(dirname "$0")"
export PATH="$HOME/.cargo/bin:$PATH"
log() { echo "[$(date +%H:%M:%S)] $*"; }
log "rust unrestricted (fresh/persistent)"
python3 engine/run.py unrestricted --engines fresh persistent --workloads empty reads query sync --sizes 0 128 --name rust-unrestricted
log "rust unrestricted (deno)"
python3 engine/run.py unrestricted --engines deno --workloads empty reads query sync --sizes 0 --name rust-deno-tiny
python3 engine/run.py unrestricted --engines deno --workloads query sync --sizes 128 --name rust-deno-large
log "bun unrestricted"
python3 bun/run.py unrestricted --name bun-unrestricted
log "primitives"
python3 engine/run.py unrestricted --engines isolate context cold cached terminate --workloads empty --sizes 128 --name rust-primitives
python3 bun/run.py unrestricted --engines context realm worker cold cached terminate vmtimeout --workloads empty --sizes 128 --name bun-primitives
python3 bun/run.py unrestricted --engines cached fresh-vm --workloads empty --sizes 128 --cache none --name bun-nocache
log "quota query"
python3 engine/run.py quota --engines fresh persistent --workloads query --sizes 0 128 --bursts 10 --init declarations --name rust-quota-query
python3 bun/run.py quota --engines inline persistent fresh-vm --workloads query --sizes 0 128 --bursts 10 --init declarations --name bun-quota-query
log "quota sync"
python3 engine/run.py quota --engines fresh persistent --workloads sync --sizes 0 128 --bursts 5 --init declarations --name rust-quota-sync
python3 bun/run.py quota --engines inline persistent fresh-vm --workloads sync --sizes 0 128 --bursts 5 --init declarations --name bun-quota-sync
log "done"
