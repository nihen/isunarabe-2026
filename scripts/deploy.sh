#!/usr/bin/env bash
# Sync local webapp/ to target server(s), build (release), and restart the service.
# Usage:
#   scripts/deploy.sh            # all 3 servers in parallel (default)
#   scripts/deploy.sh 1          # only nrb2026-1
#   scripts/deploy.sh 1 3        # nrb2026-1 and -3
#   PROFILE=debug scripts/deploy.sh   # cargo run (debug, current default systemd unit)
#   PROFILE=release scripts/deploy.sh # cargo build --release; expects unit pointing at target/release/webapp
#
# Notes:
# - seed.sql is excluded — it's not pushed on every deploy. Use scripts/seed-fetch.sh / seed-push.sh.
# - public/ is the SPA, regulation forbids editing it but we still rsync to keep parity.
set -euo pipefail

cd "$(dirname "$0")/.."
source scripts/hosts.sh

PROFILE="${PROFILE:-debug}"
SERVICE="nrb2026-webapp.service"

deploy_one() {
  local host="$1"
  local tag="${host}"

  echo "[deploy ${tag}] rsync -> ${host}"
  rsync -az --delete \
    --exclude target \
    --exclude .git \
    --exclude 'sql/seed.sql' \
    -e "ssh ${SSH_OPTS[*]}" \
    ./webapp/ "${SSH_USER}@${host}:/home/isucon/webapp/"

  if [ "$PROFILE" = "release" ]; then
    echo "[deploy ${tag}] cargo build --release"
    ssh "${SSH_OPTS[@]}" "${SSH_USER}@${host}" \
      'cd ~/webapp && ~/.cargo/bin/cargo build --release --bin webapp'
  fi

  echo "[deploy ${tag}] systemctl restart ${SERVICE}"
  ssh "${SSH_OPTS[@]}" "${SSH_USER}@${host}" "sudo systemctl restart ${SERVICE}"

  echo "[deploy ${tag}] done."
}

# Resolve targets
targets=()
if [ "$#" -eq 0 ]; then
  mapfile -t targets < <(resolve_targets all)
else
  for arg in "$@"; do
    targets+=("$(resolve_targets "$arg")")
  done
fi

echo "[deploy] profile=${PROFILE}, targets: ${targets[*]}"

# Run in parallel, fail if any fails.
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
