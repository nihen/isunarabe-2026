#!/usr/bin/env bash
# Configure nrb2026-3 as the MySQL host (it also keeps running webapp,
# so /api/initialize can be hit against this host with mysql at localhost — fast seed load).
#  - Push etc/db/ -> /etc/  (mysql.conf.d/isucon.cnf, etc.)
#  - Create user 'isucon'@'192.168.0.%' (mysql_native_password)
#  - Restart mysql.service
#
# Note: webapp on this host is intentionally kept enabled with DEFAULT DATABASE_URL
# (= 127.0.0.1) so that an initialize run targeted here does not pay any network cost.
set -euo pipefail

cd "$(dirname "$0")/.."
source scripts/hosts.sh

host="$DB_SERVER"
echo "[setup-db] target: ${host}  (webapp + mysql)"

# 1) Push /etc files for the db role (mysql config etc.).
scripts/deploy-config.sh db

# 2) Create remote-accessible isucon user (idempotent).
ssh "${SSH_OPTS[@]}" "${SSH_USER}@${host}" "sudo mysql <<'SQL'
CREATE USER IF NOT EXISTS 'isucon'@'192.168.0.%' IDENTIFIED WITH mysql_native_password BY 'isucon';
GRANT ALL PRIVILEGES ON *.* TO 'isucon'@'192.168.0.%' WITH GRANT OPTION;
FLUSH PRIVILEGES;
SQL
"

# 3) Restart mysql to apply bind-address etc.
ssh "${SSH_OPTS[@]}" "${SSH_USER}@${host}" 'sudo systemctl restart mysql.service'

# 4) Smoke test: localhost (from db host) and remote (from app host).
echo "[setup-db] local check:"
ssh "${SSH_OPTS[@]}" "${SSH_USER}@${host}" \
  "MYSQL_PWD=isucon mysql -h 127.0.0.1 -P 3306 -u isucon --protocol=TCP -e 'SELECT @@hostname AS host, @@bind_address AS bind;' nrb2026"

app1="${APP_SERVERS[0]}"
echo "[setup-db] remote check from ${app1}:"
ssh "${SSH_OPTS[@]}" "${SSH_USER}@${app1}" \
  "MYSQL_PWD=isucon mysql -h ${DB_PRIVATE_IP} -P 3306 -u isucon --protocol=TCP -e 'SELECT @@hostname AS host, @@bind_address AS bind;' nrb2026"

echo "[setup-db] done."
