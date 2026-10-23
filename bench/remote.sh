#!/usr/bin/env bash
# Runs on the bench server (see run.sh): installs the Node and Pi each setup's lock pins, then
# builds, verifies and benches it. Results land in ~/bench/results/.
set -euo pipefail
cd ~/bench
mkdir -p results
date=$(date -u +%F)

lock_version() { sed -n "/^\[$1\]/,/^\[/s/^version = \"\(.*\)\"/\1/p" "$2"; }

for setup in setups/*/; do
  name=$(basename "$setup")
  node=$(lock_version node "$setup/pibake.lock")
  pi=$(lock_version pi "$setup/pibake.lock")

  if [ ! -x "/opt/node-v$node/bin/node" ]; then
    curl -fsSL "https://nodejs.org/dist/v$node/node-v$node-linux-x64.tar.xz" | tar -xJ -C /opt
    mv "/opt/node-v$node-linux-x64" "/opt/node-v$node"
  fi
  export PATH=/opt/node-v$node/bin:$PATH
  [ "$(pi --version 2>/dev/null)" = "$pi" ] || npm install -g "@earendil-works/pi-coding-agent@$pi"

  (cd "$setup/agent/npm" && npm ci --legacy-peer-deps --no-audit --no-fund)

  perf=()
  perf stat -e instructions:u true >/dev/null 2>&1 && perf=(--perf-stat)
  scenarios=()
  [ -n "${SCENARIOS:-}" ] && scenarios=(--scenarios "$SCENARIOS")

  cd "$setup"
  baked=$PWD/target/pibake/bin/pi-baked
  ~/bench/pibake build
  ~/bench/pibake verify --pi "$baked"
  ~/bench/pibake bench --pi pi --pi "$baked" --runs "$RUNS" --record "hetzner_$name" "${perf[@]}" "${scenarios[@]}" \
    | tee ~/bench/results/"${date}_hetzner_$name.txt"
  cp bench_results/*_hetzner_"$name".json ~/bench/results/
  cd ~/bench

  {
    echo "pibake commit: $PIBAKE_COMMIT"
    echo "perf counters: ${perf[*]:-unavailable}"
    echo "node: $(node --version), pi: $(pi --version)"
    uname -a
    grep PRETTY_NAME /etc/os-release
    lscpu
  } > results/"${date}_hetzner_${name}_machine.txt"
done
