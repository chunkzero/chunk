#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
python3 engine/run.py unrestricted --engines persistent tuned actor --workloads query sync --sizes 0 128 --name rust-opt-unrestricted
python3 engine/run.py quota --engines persistent tuned actor --workloads query --sizes 0 128 --bursts 10 --init declarations --name rust-opt-quota-query
python3 engine/run.py quota --engines persistent tuned actor --workloads sync --sizes 0 128 --bursts 5 --init declarations --name rust-opt-quota-sync
echo OPT-DONE
