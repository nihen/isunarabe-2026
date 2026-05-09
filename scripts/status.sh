#!/usr/bin/env bash
# Show service status on all servers.
# Usage: scripts/status.sh [1|2|3|all]   (default: all)
set -euo pipefail

cd "$(dirname "$0")/.."
source scripts/hosts.sh

mapfile -t targets < <(resolve_targets "${1:-all}")

for host in "${targets[@]}"; do
  echo "================ ${host} ================"
  ssh "${SSH_OPTS[@]}" "${SSH_USER}@${host}" \
    'systemctl is-active nrb2026-webapp.service mysql.service; \
     systemctl --no-pager --lines=5 status nrb2026-webapp.service | head -20'
  echo
done
