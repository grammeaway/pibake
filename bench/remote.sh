#!/usr/bin/env bash
# Runs on the bench server (see run.sh): installs the Node and Pi each setup's lock pins, then
# builds and verifies it on the default backend and on sea_snapshot, and benches both against
# stock Pi. Results land in ~/bench/results/.
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

  if [ -d "$setup/agent/npm" ]; then
    (cd "$setup/agent/npm" && npm ci --legacy-peer-deps --no-audit --no-fund)
  fi

  perf=()
  perf stat -e instructions:u true >/dev/null 2>&1 && perf=(--perf-stat)
  scenarios=()
  [ -n "${SCENARIOS:-}" ] && scenarios=(--scenarios "$SCENARIOS")

  snapshot_setup=~/bench/snapshot/$name
  mkdir -p ~/bench/snapshot
  rsync -a --delete --exclude target --exclude bench_results "$setup" "$snapshot_setup/"
  printf '\n[build]\nbackend = "sea_snapshot"\nname = "pi-snapshot"\n' >> "$snapshot_setup/pibake.toml"
  (cd "$snapshot_setup" && ~/bench/pibake build && ~/bench/pibake verify --pi "$PWD/target/pibake/bin/pi-snapshot")
  snapshot=$snapshot_setup/target/pibake/bin/pi-snapshot

  cd "$setup"
  baked=$PWD/target/pibake/bin/pi-baked
  ~/bench/pibake build
  ~/bench/pibake verify --pi "$baked"
  ~/bench/pibake bench --pi pi --pi "$baked" --pi "$snapshot" --runs "$RUNS" --record "hetzner_$name" "${perf[@]}" "${scenarios[@]}" \
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
