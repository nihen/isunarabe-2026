#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
MYSQL_IMAGE="${MYSQL_IMAGE:-mysql:8.0}"
MYSQL_CONTAINER="${MYSQL_CONTAINER:-nrb2026-regtest-mysql}"
MYSQL_PORT="${MYSQL_PORT:-13306}"
APP_PORT="${APP_PORT:-18080}"
DB_URL="mysql://isucon:isucon@127.0.0.1:${MYSQL_PORT}/nrb2026"
TMP_SQL_DIR="$(mktemp -d)"
APP_LOG="${TMPDIR:-/tmp}/nrb2026-regtest-app.log"
APP_PID=""

cleanup() {
    if [[ -n "${APP_PID}" ]] && kill -0 "${APP_PID}" 2>/dev/null; then
        kill "${APP_PID}" 2>/dev/null || true
        wait "${APP_PID}" 2>/dev/null || true
    fi
    docker rm -f "${MYSQL_CONTAINER}" >/dev/null 2>&1 || true
    rm -rf "${TMP_SQL_DIR}"
}
trap cleanup EXIT

cp "${ROOT_DIR}/sql/schema.sql" "${TMP_SQL_DIR}/schema.sql"
cp "${ROOT_DIR}/sql/seed.base.sql" "${TMP_SQL_DIR}/seed.base.sql"

docker rm -f "${MYSQL_CONTAINER}" >/dev/null 2>&1 || true
docker run -d \
    --name "${MYSQL_CONTAINER}" \
    -e MYSQL_ROOT_PASSWORD=root \
    -e MYSQL_DATABASE=nrb2026 \
    -e MYSQL_USER=isucon \
    -e MYSQL_PASSWORD=isucon \
    -p "${MYSQL_PORT}:3306" \
    "${MYSQL_IMAGE}" >/dev/null

for _ in $(seq 1 120); do
    if MYSQL_PWD=isucon mysql \
        -h 127.0.0.1 \
        -P "${MYSQL_PORT}" \
        -u isucon \
        --protocol=TCP \
        -e 'SELECT 1' nrb2026 >/dev/null 2>&1; then
        break
    fi
    sleep 1
done

MYSQL_PWD=isucon mysql \
    -h 127.0.0.1 \
    -P "${MYSQL_PORT}" \
    -u isucon \
    --protocol=TCP \
    -e 'SELECT 1' nrb2026 >/dev/null

DATABASE_URL="${DB_URL}" \
SQL_DIR="${TMP_SQL_DIR}" \
PORT="${APP_PORT}" \
cargo run --manifest-path "${ROOT_DIR}/Cargo.toml" >"${APP_LOG}" 2>&1 &
APP_PID="$!"

BASE_URL="http://127.0.0.1:${APP_PORT}" python3 "${ROOT_DIR}/tests/regulation_api_test.py"
