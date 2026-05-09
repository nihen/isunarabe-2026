#!/usr/bin/env bash
# Configure nrb2026-1, nrb2026-2 as app-only hosts:
#  - Push etc/app/ -> /etc/  (systemd drop-in pointing DATABASE_URL at 192.168.0.13, etc.)
#  - Stop & disable mysql.service (free RAM)
#  - daemon-reload + restart nrb2026-webapp.service
set -euo pipefail

cd "$(dirname "$0")/.."
source scripts/hosts.sh

# 1) Push only systemd drop-ins for now (nginx not yet installed; flip is Phase 2).
scripts/deploy-config.sh app systemd

# 2) Per-host: disable mysql, reload, restart webapp.
post_one() {
  local host="$1"
  echo "[setup-app ${host}] disable mysql, daemon-reload, restart webapp"
  ssh "${SSH_OPTS[@]}" "${SSH_USER}@${host}" '
    set -e
    sudo systemctl disable --now mysql.service || true
    sudo systemctl daemon-reload
    sudo systemctl restart nrb2026-webapp.service
  '
  echo "[setup-app ${host}] verify DATABASE_URL:"
  ssh "${SSH_OPTS[@]}" "${SSH_USER}@${host}" \
    "systemctl show nrb2026-webapp.service -p Environment | tr ' ' '\n' | grep DATABASE_URL || echo '(no override)'"
}

pids=()
for host in "${APP_SERVERS[@]}"; do
  post_one "$host" &
  pids+=($!)
done
rc=0
for pid in "${pids[@]}"; do wait "$pid" || rc=1; done
[ "$rc" -eq 0 ] || { echo "[setup-app] FAILED" >&2; exit 1; }
echo "[setup-app] done."
