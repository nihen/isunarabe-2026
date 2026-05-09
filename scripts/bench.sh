#!/usr/bin/env bash
# Open the ISUNARABE portal in the default browser.
# Per regulation §6.2, bench runs must be triggered from the portal — direct invocation
# of the bench binary does not record to the leaderboard.
#
# Usage:
#   scripts/bench.sh           # open portal
#   scripts/bench.sh logs      # tail benchwarmer.service on bench server (live during a run)
set -euo pipefail

cd "$(dirname "$0")/.."
source scripts/hosts.sh

case "${1:-open}" in
  open)
    url="https://isunarabe.org/"
    echo "[bench] opening ${url}"
    if command -v open >/dev/null 2>&1; then
      open "$url"
    else
      echo "$url"
    fi
    echo "[bench] then click 'ベンチマーク実行' on your contest page."
    ;;
  logs)
    echo "[bench] tailing benchwarmer on ${BENCH}"
    ssh "${SSH_OPTS[@]}" -t "${SSH_USER}@${BENCH}" \
      'sudo journalctl -fu benchwarmer.service -n 200'
    ;;
  *)
    echo "usage: $0 [open|logs]" >&2
    exit 1
    ;;
esac
