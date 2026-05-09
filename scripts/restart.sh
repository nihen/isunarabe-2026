#!/usr/bin/env bash
# Restart nrb2026-webapp.service on target server(s) without rsync/build.
# Usage: scripts/restart.sh [1|2|3|all]   (default: all)
set -euo pipefail

cd "$(dirname "$0")/.."
source scripts/hosts.sh

SERVICE="nrb2026-webapp.service"

mapfile -t targets < <(resolve_targets "${1:-all}")

for host in "${targets[@]}"; do
  echo "[restart] ${host}"
  ssh "${SSH_OPTS[@]}" "${SSH_USER}@${host}" "sudo systemctl restart ${SERVICE}" &
done
wait
echo "[restart] done."
