#!/usr/bin/env bash
# Mirror local etc/<role>/[subpath/] to /etc/[subpath/] on target host(s) via sudo rsync.
#
# Usage:
#   scripts/deploy-config.sh                       # both roles, full etc/<role>/
#   scripts/deploy-config.sh app                   # all of etc/app/   -> APP_SERVERS:/etc/
#   scripts/deploy-config.sh db                    # all of etc/db/    -> DB_SERVER:/etc/
#   scripts/deploy-config.sh app systemd           # only etc/app/systemd/ -> /etc/systemd/
#   scripts/deploy-config.sh app nginx             # only etc/app/nginx/   -> /etc/nginx/
#
# Notes:
# - Uses rsync --rsync-path="sudo rsync" so files end up root-owned.
# - Does NOT trigger reloads. Each setup-* script handles its own post-deploy actions.
# - Idempotent. Will not delete remote files (no --delete) — manual cleanup if needed.
set -euo pipefail

cd "$(dirname "$0")/.."
source scripts/hosts.sh

push_to_host() {
  local role="$1" host="$2" subpath="${3:-}"
  local src dest
  if [ -n "$subpath" ]; then
    src="etc/${role}/${subpath}/"
    dest="/etc/${subpath}/"
  else
    src="etc/${role}/"
    dest="/etc/"
  fi
  if [ ! -d "$src" ]; then
    echo "[deploy-config ${host}] no such source ${src}, skipping"
    return
  fi
  if [ -z "$(ls -A "$src" 2>/dev/null)" ]; then
    echo "[deploy-config ${host}] ${src} is empty, skipping"
    return
  fi
  echo "[deploy-config ${host}] rsync ${src} -> ${dest}  (role=${role})"
  rsync -az \
    --rsync-path="sudo rsync" \
    -e "ssh ${SSH_OPTS[*]}" \
    "$src" "${SSH_USER}@${host}:${dest}"
}

target="${1:-both}"
sub="${2:-}"

case "$target" in
  app)
    for h in "${APP_SERVERS[@]}"; do push_to_host app "$h" "$sub"; done
    ;;
  db)
    push_to_host db "$DB_SERVER" "$sub"
    ;;
  both|all|"")
    for h in "${APP_SERVERS[@]}"; do push_to_host app "$h" "$sub"; done
    push_to_host db "$DB_SERVER" "$sub"
    ;;
  *)
    echo "usage: $0 [app|db|both] [subpath]" >&2
    exit 1
    ;;
esac

echo "[deploy-config] done."
