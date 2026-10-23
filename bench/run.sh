#!/usr/bin/env bash
# Benches stock Pi against a pibake build of each setup in bench/setups/ on the server from
# bench/infra (`terraform apply` there first), then copies results to bench_results/hetzner/.
# RUNS (default 20) sets runs per scenario; SCENARIOS (comma-separated, default all) picks them.
#
# The run lives in a tmux session on the server, so a dropped connection loses nothing: rerun
# this script to follow it again, and it fetches the results once the run ends. A finished
# server is not run twice; destroy and apply it for a fresh run.
set -euo pipefail
cd "$(dirname "$0")/.."

host=root@$(terraform -chdir=bench/infra output -raw ipv4)
# Throwaway server, new host key on every apply.
ssh=(ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR
  -o ConnectTimeout=10 -o ServerAliveInterval=30)

until "${ssh[@]}" "$host" true 2>/dev/null; do
  echo "waiting for ssh on $host"
  sleep 5
done
# Exit 2 is done with warnings.
"${ssh[@]}" "$host" cloud-init status --wait >/dev/null || [ $? -eq 2 ]

state=$("${ssh[@]}" "$host" \
  'if tmux has-session -t bench 2>/dev/null; then echo running; elif [ -f bench/exit ]; then echo done; fi')
if [ -z "$state" ]; then
  cargo build --release --locked --target x86_64-unknown-linux-musl
  rsync -e "${ssh[*]}" -a --delete --exclude target --exclude node_modules --exclude bench_results \
    bench/setups bench/remote.sh target/x86_64-unknown-linux-musl/release/pibake "$host:bench/"
  commit=$(git rev-parse HEAD)$(git diff --quiet HEAD -- . ':!bench_results' || echo -dirty)
  "${ssh[@]}" "$host" "tmux new-session -d -s bench \
    'RUNS=${RUNS:-20} SCENARIOS=${SCENARIOS:-} PIBAKE_COMMIT=$commit bash bench/remote.sh > bench/run.log 2>&1; echo \$? > bench/exit'"
  echo "started; Ctrl+C stops following, not the run"
fi

"${ssh[@]}" "$host" 'tail -n +1 -F bench/run.log 2>/dev/null & while tmux has-session -t bench 2>/dev/null; do sleep 5; done; kill $!'

mkdir -p bench_results/hetzner
rsync -e "${ssh[*]}" -a "$host:bench/results/" bench_results/hetzner/
code=$("${ssh[@]}" "$host" cat bench/exit)
echo "remote run exited $code; results in bench_results/hetzner/"
exit "$code"
