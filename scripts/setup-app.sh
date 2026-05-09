#!/usr/bin/env bash
# Push webapp config (systemd drop-in) to ALL 3 servers.
# Disable mysql on nrb2026-1, nrb2026-2 (memory saving).
set -euo pipefail

cd "$(dirname "$0")/.."
source scripts/hosts.sh

# 1) Push systemd drop-in (DATABASE_URL=192.168.0.13) to all 3.
scripts/deploy-config.sh app systemd

# 2) Reload + restart webapp on all 3.
for host in "${APP_SERVERS[@]}"; do
  (
    echo "[setup-app ${host}] daemon-reload + restart webapp"
    ssh "${SSH_OPTS[@]}" "${SSH_USER}@${host}" '
      sudo systemctl daemon-reload
      sudo systemctl restart nrb2026-webapp.service
    '
  ) &
done
wait

# 3) Disable mysql on 1, 2 only.
for host in "${NO_MYSQL[@]}"; do
  (
    echo "[setup-app ${host}] disable mysql"
    ssh "${SSH_OPTS[@]}" "${SSH_USER}@${host}" \
      'sudo systemctl disable --now mysql.service || true'
  ) &
done
wait

echo "[setup-app] done."
