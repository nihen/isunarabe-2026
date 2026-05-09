#!/usr/bin/env bash
# Deploy webapp to target server(s): rsync + cargo build --release + restart.
#
# Usage:
#   scripts/deploy.sh            # authority (1) + replica (2) in parallel
#   scripts/deploy.sh 1          # only nrb2026-1
#   scripts/deploy.sh 2          # only nrb2026-2
#   scripts/deploy.sh 1 2        # both
#   scripts/deploy.sh all        # all 3 (including DB host)
set -euo pipefail

cd "$(dirname "$0")/.."
source scripts/hosts.sh

SERVICE="nrb2026-webapp.service"

deploy_one() {
  local host="$1"
  echo "[deploy ${host}] rsync"
  rsync -az --delete \
    --exclude target --exclude .git --exclude 'sql/seed.sql' \
    -e "ssh ${SSH_OPTS[*]}" \
    ./webapp/ "${SSH_USER}@${host}:/home/isucon/webapp/"

  echo "[deploy ${host}] cargo build --release"
  ssh "${SSH_OPTS[@]}" "${SSH_USER}@${host}" \
    'cd ~/webapp && ~/.cargo/bin/cargo build --release --bin webapp 2>&1 | tail -2 && \
     cp -f target/release/webapp target/release/webapp.prev 2>/dev/null; \
     cp target/release/webapp target/release/webapp.new && \
     mv -f target/release/webapp.new target/release/webapp'

  echo "[deploy ${host}] restart"
  ssh "${SSH_OPTS[@]}" "${SSH_USER}@${host}" "sudo systemctl restart ${SERVICE}"
  echo "[deploy ${host}] done"
}

# Default: authority (1) + replica (2)
targets=()
if [ "$#" -eq 0 ]; then
  targets=("${SERVERS[0]}" "${SERVERS[1]}")
else
  for arg in "$@"; do
    mapfile -t resolved < <(resolve_targets "$arg")
    targets+=("${resolved[@]}")
  done
fi

echo "[deploy] targets: ${targets[*]}"

pids=()
for host in "${targets[@]}"; do
  deploy_one "$host" &
  pids+=($!)
done

rc=0
for pid in "${pids[@]}"; do
  wait "$pid" || rc=1
done

if [ "$rc" -ne 0 ]; then
  echo "[deploy] FAILED" >&2
  exit 1
fi

echo "[deploy] all done."
