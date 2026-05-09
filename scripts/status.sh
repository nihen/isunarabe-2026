#!/usr/bin/env bash
# Show service status, role-aware:
#   APP_SERVERS: nrb2026-webapp only (mysql is disabled)
#   DB_SERVER:   nrb2026-webapp + mysql
# Usage: scripts/status.sh [1|2|3|all]   (default: all)
set -uo pipefail

cd "$(dirname "$0")/.."
source scripts/hosts.sh

mapfile -t targets < <(resolve_targets "${1:-all}")

for host in "${targets[@]}"; do
  echo "================ ${host} ================"
  if [ "$host" = "$DB_SERVER" ]; then
    services="nrb2026-webapp.service mysql.service"
  else
    services="nrb2026-webapp.service"
  fi
  ssh "${SSH_OPTS[@]}" "${SSH_USER}@${host}" "
    echo '-- is-active --'
    for s in ${services}; do
      printf '%-30s %s\n' \"\$s\" \"\$(systemctl is-active \"\$s\" 2>&1 || true)\"
    done
    echo
    echo '-- nrb2026-webapp (last 5 lines) --'
    systemctl --no-pager --lines=3 status nrb2026-webapp.service | head -15
  "
  echo
done
