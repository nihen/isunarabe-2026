#!/usr/bin/env bash
# Fetch the (large) seed.sql from a server into local webapp/sql/seed.sql.
# Used as one-shot bootstrap; the file is gitignored.
set -euo pipefail

cd "$(dirname "$0")/.."
source scripts/hosts.sh

idx="${1:-1}"
host=$(resolve_targets "$idx")

echo "[seed-fetch] from ${host} (236M, takes a moment)..."
rsync -az --progress \
  -e "ssh ${SSH_OPTS[*]}" \
  "${SSH_USER}@${host}:/home/isucon/webapp/sql/seed.sql" \
  ./webapp/sql/seed.sql

echo "[seed-fetch] -> webapp/sql/seed.sql"
