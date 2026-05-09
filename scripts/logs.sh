#!/usr/bin/env bash
# Tail the webapp service log on a single server.
# Usage: scripts/logs.sh [1|2|3]   (default: 1)
set -euo pipefail

cd "$(dirname "$0")/.."
source scripts/hosts.sh

idx="${1:-1}"
host=$(resolve_targets "$idx")

echo "[logs] ${host} :: journalctl -fu nrb2026-webapp.service"
ssh "${SSH_OPTS[@]}" -t "${SSH_USER}@${host}" \
  'sudo journalctl -fu nrb2026-webapp.service -n 100'
