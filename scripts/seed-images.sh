#!/usr/bin/env bash
# Extract fixed campaign images from seed.sql and optionally distribute them.
#
# Usage:
#   scripts/seed-images.sh extract       # webapp/sql/seed.sql -> webapp/images/seed
#   scripts/seed-images.sh deploy all    # extract, then rsync to targets
set -euo pipefail

cd "$(dirname "$0")/.."
source scripts/hosts.sh

cmd="${1:-extract}"
target_arg="${2:-all}"
seed_sql="webapp/sql/seed.sql"
out_dir="webapp/images/seed"

if [[ ! -f "${seed_sql}" ]]; then
  echo "${seed_sql} not found. Run scripts/seed-fetch.sh first." >&2
  exit 1
fi

extract() {
  python3 webapp/scripts/extract_seed_images.py "${seed_sql}" "${out_dir}"
}

deploy_one() {
  local host="$1"
  echo "[seed-images] rsync ${out_dir}/ -> ${host}:/home/isucon/webapp/images/seed/"
  ssh "${SSH_OPTS[@]}" "${SSH_USER}@${host}" 'mkdir -p /home/isucon/webapp/images/seed'
  rsync -az --delete \
    -e "ssh ${SSH_OPTS[*]}" \
    "${out_dir}/" "${SSH_USER}@${host}:/home/isucon/webapp/images/seed/"
}

case "${cmd}" in
  extract)
    extract
    ;;
  deploy)
    extract
    mapfile -t targets < <(resolve_targets "${target_arg}")
    for host in "${targets[@]}"; do
      deploy_one "${host}" &
    done
    wait
    ;;
  *)
    echo "usage: $0 extract|deploy [target]" >&2
    exit 1
    ;;
esac
