#!/usr/bin/env bash
# Apply the app/app/mysql topology in order: DB first, then apps.
# Idempotent — safe to re-run.
set -euo pipefail

cd "$(dirname "$0")/.."

echo "==> 1/2 setup-db (nrb2026-3 -> MySQL only)"
scripts/setup-db.sh

echo "==> 2/2 setup-app (nrb2026-1,2 -> webapp pointing at DB)"
scripts/setup-app.sh

echo
echo "==> status"
scripts/status.sh all
