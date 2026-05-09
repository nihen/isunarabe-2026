#!/usr/bin/env bash
# Pull /home/isucon/webapp/ from a server into local webapp/.
# Usage: scripts/pull.sh [1|2|3]   (default: 1)
set -euo pipefail

cd "$(dirname "$0")/.."
source scripts/hosts.sh

idx="${1:-1}"
host=$(resolve_targets "$idx")

echo "[pull] from ${SSH_USER}@${host}:/home/isucon/webapp/"
rsync -az --delete \
  --exclude target \
  --exclude .git \
  -e "ssh ${SSH_OPTS[*]}" \
  "${SSH_USER}@${host}:/home/isucon/webapp/" ./webapp/

echo "[pull] done."
