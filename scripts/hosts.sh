#!/usr/bin/env bash
# Server definitions for isunarabe-2026 (team-7).
# IPs are EIPs from the CloudFormation stack and are stable for the stack lifetime.

# nrb2026-{1,2,3} (competition servers): private 192.168.0.{11,12,13}
SERVERS=(
  "3.115.92.38"     # nrb2026-1 (INSTANCE_INDEX=0)
  "35.76.59.65"     # nrb2026-2 (INSTANCE_INDEX=1)
  "35.72.99.228"    # nrb2026-3 (INSTANCE_INDEX=2)
)

# nrb2026-bench: private 192.168.0.100
BENCH="13.193.122.121"

# Topology: app, app, mysql (Phase 1).
# nrb2026-1,-2 run nrb2026-webapp; nrb2026-3 is DB-only.
APP_SERVERS=("${SERVERS[0]}" "${SERVERS[1]}")    # 3.115.92.38, 35.76.59.65
DB_SERVER="${SERVERS[2]}"                         # 35.72.99.228
DB_PRIVATE_IP="192.168.0.13"

SSH_USER="isucon"
SSH_OPTS=(-o StrictHostKeyChecking=accept-new -o ConnectTimeout=10 -o ServerAliveInterval=30)

# Resolve server arg:
#   1|2|3      -> single IP
#   all        -> all three
#   app        -> app servers (1,2)
#   db         -> DB server (3)
#   bench      -> benchmarker
resolve_targets() {
  local arg="${1:-app}"
  case "$arg" in
    1|2|3) echo "${SERVERS[$((arg-1))]}" ;;
    all) printf '%s\n' "${SERVERS[@]}" ;;
    app|"") printf '%s\n' "${APP_SERVERS[@]}" ;;
    db) echo "$DB_SERVER" ;;
    bench) echo "$BENCH" ;;
    *) echo "unknown target: $arg" >&2; return 1 ;;
  esac
}
