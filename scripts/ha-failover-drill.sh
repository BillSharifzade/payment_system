#!/usr/bin/env bash
# Failover drill for the Patroni cluster (deploy/ha): proves, under continuous
# transfer load through the real payment-server and payment-workers, that the
# database tier survives losing its leader with ZERO acknowledged-transaction
# loss and recovers on its own.
#
#   scripts/ha-failover-drill.sh [--mode local|compose] [--rounds N]
#                                [--scenarios crash,switchover,freeze[,quorum-loss]]
#                                [--rps N] [--keep]
#
# Scenarios (each under load; the leader is whichever node leads at the time):
#   crash        SIGKILL the leader's Patroni AND Postgres (a machine dying); it is
#                restarted once a new leader serves ("the machine reboots").
#   switchover   planned: patronictl switchover to a quorum standby.
#   freeze       SIGSTOP the whole leader node (a hang / partition), thaw it once a
#                new leader serves. Right after the thaw a client connected
#                DIRECTLY to the old leader tries to commit: it must not be
#                acknowledged (the old leader has no quorum standby left).
#   quorum-loss  (opt-in) both standbys cut off for HA_QUORUM_LOSS_HANG_SECS (8, i.e.
#                longer than the app's DB_STATEMENT_TIMEOUT_MS), then the leader
#                dies. Nothing may be acknowledged while no standby can confirm it,
#                no cut-off standby may be promoted, and the cluster must recover
#                when the old leader returns.
#
# After every scenario the drill asserts, and exits non-zero on any violation:
#   - a new leader within HA_LEADER_TIMEOUT (switchover: the chosen candidate);
#   - zero data loss: every transfer the client got a 2xx for exists, with its
#     entries, on EVERY member (the new leader, the other standby and the
#     rejoined old leader);
#   - conservation (every currency sums to zero) and balances = entries;
#   - `payment-workers verify-chain` reports the checkpoint chain intact;
#   - the workers re-elect a leader (exactly one advisory-lock holder on the new
#     primary), keep sealing (checkpoints newer than the fault, nothing left
#     unsealed) and the outbox relay drains;
#   - the old leader rejoins as a streaming replica on the new timeline;
#   - no split brain: never two members answering /primary, and the old leader
#     refuses writes once demoted;
#   - WAL archiving works from the new leader (`pgbackrest check`) and the
#     promotion started a backup on the new timeline.
# Finally, a point-in-time recovery from the WAL archive ALONE (the base backup
# taken before the first scenario, replayed across every failover) must hold
# every acknowledged transfer of every scenario.
# It reports, per scenario, the time to a new leader and the client's view:
# the longest stretch without an acknowledged transfer (write outage), the
# error window and the status codes seen (results.jsonl + a summary).
#
# Modes:
#   local    (default) scripts/ha-local.sh: etcd, Patroni, Postgres 16, pgBackRest
#            and HAProxy as local processes (run as root; see that script for the
#            tools). The app is built with cargo (CARGO_TARGET_DIR, default
#            ./target) unless PAYMENT_BIN_DIR holds payment-server and
#            payment-workers. Uses Redis HA_REDIS_URL (redis://127.0.0.1:6379/4)
#            and NATS HA_NATS_URL (nats://127.0.0.1:4222, subject prefix hadrill)
#            when reachable.
#   compose  the real overlay: docker-compose.prod.yml + ha/docker-compose.ha.yml
#            + ha/docker-compose.ha-drill.yml (test-only: publishes the members on
#            127.0.0.1) in a throwaway copy of deploy/; image
#            payment-system:$IMAGE_TAG (default ci) must exist, payment-patroni is
#            built unless present. Needs docker, psql, curl, jq, openssl.
# Both modes expose the same addresses: HAProxy 127.0.0.1:5450 (rw) / :7450
# (admin), members 127.0.0.1:5451-5453 (Postgres) / :8451-8453 (Patroni REST),
# API 127.0.0.1:18450, the PITR instance 127.0.0.1:5459.
#
# Knobs: HA_RPS (40), HA_WARMUP_SECS (8), HA_SETTLE_SECS (8), HA_LEADER_TIMEOUT (90),
# HA_REJOIN_TIMEOUT (180), HA_QUORUM_LOSS_HANG_SECS (8), HA_WORKDIR (/tmp/ha-drill),
# HA_DRILL_OUT (per-scenario logs and the summary; default HA_WORKDIR/../ha-drill-out).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
MODE=local ROUNDS=1 SCENARIOS=crash,switchover,freeze KEEP=0
RPS=${HA_RPS:-40}
while [[ $# -gt 0 ]]; do
  case "$1" in
    --mode) MODE=${2:?}; shift 2 ;;
    --rounds) ROUNDS=${2:?}; shift 2 ;;
    --scenarios) SCENARIOS=${2:?}; shift 2 ;;
    --rps) RPS=${2:?}; shift 2 ;;
    --keep) KEEP=1; shift ;;
    -h|--help) sed -n '2,66p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1 (see --help)" >&2; exit 2 ;;
  esac
done
[[ "$MODE" =~ ^(local|compose)$ ]] || { echo "--mode must be local or compose" >&2; exit 2; }
[[ "$ROUNDS" =~ ^[1-9][0-9]*$ ]] || { echo "--rounds must be a positive integer" >&2; exit 2; }
[[ "$RPS" =~ ^[1-9][0-9]*$ ]] || { echo "--rps must be a positive integer" >&2; exit 2; }
IFS=, read -r -a SCENARIO_LIST <<< "$SCENARIOS"
for s in "${SCENARIO_LIST[@]}"; do
  [[ "$s" =~ ^(crash|switchover|freeze|quorum-loss)$ ]] || { echo "unknown scenario: $s" >&2; exit 2; }
done

W=${HA_WORKDIR:-/tmp/ha-drill}
OUT=${HA_DRILL_OUT:-$(dirname "$W")/ha-drill-out}
WARMUP=${HA_WARMUP_SECS:-8}
SETTLE=${HA_SETTLE_SECS:-8}
LEADER_TIMEOUT=${HA_LEADER_TIMEOUT:-90}
QL_HANG=${HA_QUORUM_LOSS_HANG_SECS:-8}
REJOIN_TIMEOUT=${HA_REJOIN_TIMEOUT:-180}
API=http://127.0.0.1:18450
DRILL_PROJECT=hadrill
PSQL=${PSQL:-psql}
NODES=(1 2 3)
VIOLATIONS=()
LOAD_PID=""
APP_PIDS=()

say()  { printf '\n=== %s\n' "$*"; }
info() { printf '    %s\n' "$*"; }
violation() { VIOLATIONS+=("$*"); printf '!!  VIOLATION: %s\n' "$*" >&2; }
die() { printf '!!  %s\n' "$*" >&2; exit 1; }
now_ms() { date +%s%3N; }
secs_since() { awk -v a="$1" -v b="$(now_ms)" 'BEGIN { printf "%.2f", (b - a) / 1000 }'; }

# ---- database access (superuser, straight to a member or through HAProxy) --------------
pgpass() { cat "$SECRETS/postgres_password"; }
sql_port() { # sql_port <port> <sql>: one value per line, unaligned
  PGPASSWORD=$(pgpass) PGCONNECT_TIMEOUT=3 "$PSQL" -X -qtA -v ON_ERROR_STOP=1 \
    -h 127.0.0.1 -p "$1" -U payment -d payment -c "$2"
}
sql_rw() { sql_port 5450 "$1"; }
sql_node() { sql_port "545$1" "$2"; }

api_get() { curl -fsS -m "${API_TIMEOUT:-2}" "http://127.0.0.1:845$1$2" 2>/dev/null; }
is_primary() { api_get "$1" /primary >/dev/null; }
leader() { local n; for n in "${NODES[@]}"; do is_primary "$n" && { echo "$n"; return 0; }; done; return 1; }
primaries() { local n c=0; for n in "${NODES[@]}"; do ! is_primary "$n" || c=$((c + 1)); done; echo "$c"; }
member_state() { api_get "$1" /patroni | jq -r '"\(.role) \(.state) \(.replication_state // "-") tl=\(.timeline // 0)"' 2>/dev/null || echo "down"; }

wait_until() { # wait_until <seconds> <cmd...>: poll every 0.25 s
  local deadline=$(( $(now_ms) + $1 * 1000 )); shift
  until "$@" >/dev/null 2>&1; do
    (( $(now_ms) < deadline )) || return 1
    sleep 0.25
  done
}

# ---- drivers ---------------------------------------------------------------------------
local_drv() { HA_WORKDIR="$W" "$ROOT/scripts/ha-local.sh" "$@"; }
COMPOSE=(docker compose --project-directory "$W/deploy" -p "$DRILL_PROJECT"
         -f "$W/deploy/docker-compose.prod.yml" -f "$W/deploy/ha/docker-compose.ha.yml"
         -f "$W/deploy/ha/docker-compose.ha-drill.yml")

app_env() { # the drill's application settings (both modes): dev mode, no device
            # binding, limits out of the way, fast worker cadences
  cat <<ENV
APP_ENV=dev
RUST_LOG=info
DEVICE_BINDING=off
DEPOSIT_DUAL_CONTROL=false
RATE_LIMIT_MAX=10000000
LOGIN_LIMIT_MAX=10000000
RESOLVE_LIMIT_MAX=10000000
AML_L1_PER_TX_MINOR=100000000000
AML_L1_DAILY_MINOR=100000000000
AML_L1_VELOCITY_PER_HOUR=100000000
AML_L2_PER_TX_MINOR=100000000000
AML_L2_DAILY_MINOR=100000000000
AML_L2_VELOCITY_PER_HOUR=100000000
WORKER_INTERVAL_SECS=1
RELAY_INTERVAL_SECS=1
VERIFY_INTERVAL_SECS=5
RECONCILE_INTERVAL_SECS=5
NATS_SUBJECT_PREFIX=hadrill
ENV
}

drv_up() {
  if [[ $MODE == local ]]; then
    local_drv up
    SECRETS=$W/secrets
  else
    rm -rf "$W"; mkdir -p "$W"
    cp -a "$ROOT/deploy" "$W/deploy"
    rm -rf "$W/deploy/secrets" "$W/deploy/.env" "$W/deploy/app.env"
    ( cd "$W/deploy"
      # shellcheck source=deploy/lib.sh
      . ./lib.sh
      generate_secrets >/dev/null
      HA_SECRETS_DIR=secrets ha/init-secrets.sh --pgbackrest-posix >/dev/null
      : > secrets/backup_recipients
      chmod 0644 secrets/*   # throwaway: readable by every container user
      { cp app.env.example app.env; app_env; } > app.env.tmp && mv app.env.tmp app.env
      printf '%s\n' "ADMIN_ALLOW_CIDR=127.0.0.1/32" "IMAGE_TAG=${IMAGE_TAG:-ci}" "SECRETS_GID=$(id -g)" > .env )
    SECRETS=$W/deploy/secrets
    docker image inspect "payment-patroni:${IMAGE_TAG:-ci}" >/dev/null 2>&1 || "${COMPOSE[@]}" build pg-1
    "${COMPOSE[@]}" up -d --wait --wait-timeout 300 payment-server
  fi
  wait_until 180 quorum_ready || die "no quorum standby after 180 s"
  # HAProxy's read-only port lands on a replica.
  wait_until 30 ro_port_on_replica || violation "HAProxy's read-only port (5454) does not reach a replica"
}

ro_port_on_replica() { [[ "$(sql_port 5454 "SELECT pg_is_in_recovery()")" == t ]]; }

quorum_ready() {
  local n
  for n in "${NODES[@]}"; do
    api_get "$n" /patroni | jq -e '.quorum_standby == true or .sync_standby == true' >/dev/null && return 0
  done
  return 1
}

drv_down() {
  if [[ $MODE == local ]]; then
    app_down
    local_drv down
  else
    "${COMPOSE[@]}" down -v --remove-orphans --timeout 5 >/dev/null 2>&1 || true
    rm -rf "$W"
  fi
}

drv_node() { # drv_node kill|start|freeze|thaw <n>
  if [[ $MODE == local ]]; then
    local_drv "$1" "$2"
  else
    case "$1" in
      kill) "${COMPOSE[@]}" kill -s SIGKILL "pg-$2" ;;
      start) "${COMPOSE[@]}" start "pg-$2" ;;
      freeze) "${COMPOSE[@]}" pause "pg-$2" ;;
      thaw) "${COMPOSE[@]}" unpause "pg-$2" ;;
    esac
  fi
}

drv_patronictl() { # drv_patronictl <via node> args...
  local via=$1; shift
  if [[ $MODE == local ]]; then
    local_drv patronictl "$@"
  else
    "${COMPOSE[@]}" exec -T "pg-$via" patronictl "$@"
  fi
}

drv_pgbackrest() { # drv_pgbackrest <node> args...: pgBackRest (stanza payment) on that member
  local n=$1; shift
  if [[ $MODE == local ]]; then
    local_drv as-node "$n" pgbackrest --stanza=payment --log-level-console=warn "$@"
  else
    "${COMPOSE[@]}" exec -T "pg-$n" pgbackrest --stanza=payment --log-level-console=warn "$@"
  fi
}

# Point-in-time recovery from the archive ALONE: the base backup taken after
# setup, restored into a scratch instance on 127.0.0.1:5459, replays every
# archived segment — across every timeline the drill created — to the end and
# promotes. PGBACKREST_PG1_PATH points pgBackRest (restore and the
# restore_command it writes) at the scratch directory.
# shellcheck disable=SC2016  # expanded by the shell that runs it
PITR_SCRIPT='set -e
mkdir -m 700 "$PGBACKREST_PG1_PATH"
pgbackrest --stanza=payment --log-level-console=warn --set="$PITR_SET" restore
exec postgres -D "$PGBACKREST_PG1_PATH" -p 5459 -c listen_addresses="$PITR_LISTEN" \
  -c unix_socket_directories="$PITR_SOCK" -c archive_mode=off -c synchronous_standby_names= \
  -c hba_file="$PGBACKREST_PG1_PATH/pg_hba.conf" -c ident_file="$PGBACKREST_PG1_PATH/pg_ident.conf" \
  -c cluster_name=pitr -c shared_buffers=128MB'
PITR_PID=""
pitr_start() {
  if [[ $MODE == local ]]; then
    local_drv as-node 1 env PGBACKREST_PG1_PATH="$W/pitr" PITR_SET="$BASE_SET" PITR_LISTEN=127.0.0.1 PITR_SOCK="$W/run" \
      bash -c "$PITR_SCRIPT" >"$OUT/pitr.log" 2>&1 &
    PITR_PID=$!
  else
    # No --rm: pitr_stop saves the container's log before removing it.
    docker rm -f "$DRILL_PROJECT-pitr" >/dev/null 2>&1 || true
    docker run -d --name "$DRILL_PROJECT-pitr" --user 999:999 -p 127.0.0.1:5459:5459 \
      -v "${DRILL_PROJECT}_pgbackrest-repo:/var/lib/pgbackrest" \
      -v "$SECRETS/pgbackrest.conf:/etc/pgbackrest/pgbackrest.conf:ro" \
      -e PGBACKREST_PG1_PATH=/tmp/pitr -e PITR_SET="$BASE_SET" -e PITR_LISTEN='*' -e PITR_SOCK=/tmp \
      --entrypoint bash "payment-patroni:${IMAGE_TAG:-ci}" -c "$PITR_SCRIPT" >/dev/null
  fi
}
pitr_stop() {
  if [[ $MODE == local ]]; then
    [[ -z "$PITR_PID" ]] && return 0
    local_drv as-node 1 pg_ctl -D "$W/pitr" -m immediate -w stop >/dev/null 2>&1 || true
    wait "$PITR_PID" 2>/dev/null || true
    PITR_PID=""
    rm -rf "$W/pitr"
  else
    docker logs "$DRILL_PROJECT-pitr" >"$OUT/pitr.log" 2>&1 || true
    docker rm -f "$DRILL_PROJECT-pitr" >/dev/null 2>&1 || true
  fi
}
pitr_promoted() { [[ "$(sql_port 5459 "SELECT NOT pg_is_in_recovery()")" == t ]]; }
timeline_backup() { # timeline_backup <node> <timeline>: a backup whose WAL starts on that timeline exists
  drv_pgbackrest "$1" info --output=json 2>/dev/null \
    | jq -e --arg tl "$(printf '%08X' "$2")" '[.[0].backup[] | select(.archive.start | startswith($tl))] | length > 0'
}

verify_chain() {
  if [[ $MODE == local ]]; then
    env -i PATH="$PATH" APP_ENV=dev DATABASE_URL="$APP_DB_URL" WORKER_SIGNING_KEY="$SIGNING_KEY" \
      WORKER_TRUSTED_PUBLIC_KEYS="$TRUSTED_KEYS" "$BIN/payment-workers" verify-chain 2>/dev/null
  else
    "${COMPOSE[@]}" run --rm --no-deps -T payment-workers verify-chain 2>/dev/null
  fi
}

# ---- the application -------------------------------------------------------------------
app_up() {
  if [[ $MODE == compose ]]; then
    "${COMPOSE[@]}" up -d --wait --wait-timeout 300 --scale payment-workers=2 payment-workers
    wait_until 120 curl -fsS "$API/ready" || die "payment-server not ready"
    return
  fi
  if [[ -n "${PAYMENT_BIN_DIR:-}" ]]; then
    BIN=$PAYMENT_BIN_DIR
  else
    BIN=${CARGO_TARGET_DIR:-$ROOT/target}/release
    say "building payment-server + payment-workers (release, $BIN)"
    ( cd "$ROOT" && cargo build --release --locked --bin payment-server --bin payment-workers )
  fi
  local app_pw owner_pw redis nats
  app_pw=$(cat "$SECRETS/pg_app_password"); owner_pw=$(cat "$SECRETS/pg_owner_password")
  APP_DB_URL="postgres://payment_app:$app_pw@127.0.0.1:5450/payment"
  SIGNING_KEY=$(openssl rand -hex 32)
  # shellcheck source=deploy/lib.sh
  TRUSTED_KEYS=$(. "$ROOT/deploy/lib.sh"; ed25519_public_hex "$SIGNING_KEY")
  redis=${HA_REDIS_URL:-redis://127.0.0.1:6379/4}
  nats=${HA_NATS_URL:-nats://127.0.0.1:4222}
  (exec 3<>"/dev/tcp/127.0.0.1/6379") 2>/dev/null || redis=""
  (exec 3<>"/dev/tcp/127.0.0.1/4222") 2>/dev/null || nats=""
  mkdir -p "$W/app/kyc"
  local common
  mapfile -t common < <(app_env)
  common+=("PATH=$PATH" "DATABASE_URL=$APP_DB_URL" "DOCUMENT_STORE_DIR=$W/app/kyc")
  env -i "${common[@]}" MIGRATION_DATABASE_URL="postgres://payment_owner:$owner_pw@127.0.0.1:5450/payment" \
    JWT_SECRET="$(openssl rand -hex 32)" BIND_ADDR=127.0.0.1:18450 METRICS_ADDR=127.0.0.1:19450 \
    ${redis:+REDIS_URL=$redis} "$BIN/payment-server" >>"$W/app/server.log" 2>&1 &
  APP_PIDS+=($!)
  wait_until 120 curl -fsS "$API/ready" || die "payment-server not ready (see $W/app/server.log)"
  local i
  for i in 1 2; do
    env -i "${common[@]}" WORKER_SIGNING_KEY="$SIGNING_KEY" WORKER_TRUSTED_PUBLIC_KEYS="$TRUSTED_KEYS" \
      METRICS_ADDR="127.0.0.1:1945$i" WORKER_HEARTBEAT_FILE="$W/app/workers-$i.heartbeat" \
      ${nats:+NATS_URL=$nats} "$BIN/payment-workers" >>"$W/app/workers-$i.log" 2>&1 &
    APP_PIDS+=($!)
  done
}

app_down() {
  [[ ${#APP_PIDS[@]} -eq 0 ]] && return 0
  kill -TERM "${APP_PIDS[@]}" 2>/dev/null || true
  wait "${APP_PIDS[@]}" 2>/dev/null || true
  APP_PIDS=()
}

# ---- data: users, wallets, funding -----------------------------------------------------
api() { # api <method> <path> <token|-> [json] [idempotency key]
  local args=(-fsS -m 30 -X "$1" "$API$2" -H 'content-type: application/json')
  [[ "$3" != - ]] && args+=(-H "Authorization: Bearer $3")
  [[ -n "${5:-}" ]] && args+=(-H "Idempotency-Key: $5")
  curl "${args[@]}" ${4:+-d "$4"}
}

setup_data() {
  say "users, wallets and funding through the API"
  local admin phones=() users=() i phone pw tok wallet dep
  admin=$(api POST /v1/auth/register - '{"phone":"992911000000","password":"drill-admin-pw-0"}' | jq -r .access_token)
  for i in $(seq 1 8); do
    phone=99291100000$i pw="drill-user-pw-$i"
    tok=$(api POST /v1/auth/register - "{\"phone\":\"$phone\",\"password\":\"$pw\"}" | jq -r .access_token)
    wallet=$(api GET /v1/wallets "$tok" | jq -r '.[0].id')
    phones+=("'$phone'")
    users+=("{\"phone\":\"$phone\",\"password\":\"$pw\",\"wallet\":\"$wallet\"}")
  done
  sql_rw "UPDATE users SET is_admin = true, kyc_level = 2 WHERE phone = '992911000000';
          UPDATE users SET kyc_level = 2 WHERE phone IN ($(IFS=,; echo "${phones[*]}"))" >/dev/null
  for i in "${!users[@]}"; do
    wallet=$(jq -r .wallet <<< "${users[$i]}")
    dep=$(api POST /v1/deposits "$admin" "{\"user_account\":\"$wallet\",\"amount_minor\":100000000,\"currency\":\"TJS\"}" \
          "$(cat /proc/sys/kernel/random/uuid)")
    [[ "$(jq -r .status <<< "$dep")" != pending_approval ]] || die "deposit needs approval (DEPOSIT_DUAL_CONTROL)"
  done
  printf '[%s]\n' "$(IFS=,; echo "${users[*]}")" > "$W/users.json"
  sql_rw "CREATE SCHEMA IF NOT EXISTS ha_drill;
          CREATE TABLE IF NOT EXISTS ha_drill.probe (id bigserial PRIMARY KEY, note text, at timestamptz DEFAULT now())" >/dev/null
  info "8 funded users; $(sql_rw "SELECT count(*) FROM transactions") transactions so far"
}

load_start() { # load_start <dir>
  mkdir -p "$1"
  python3 -I "$ROOT/scripts/ha-load.py" run --api "$API" --users "$W/users.json" --out "$1" \
    --rps "$RPS" --threads 8 >>"$1/load.log" 2>&1 &
  LOAD_PID=$!
}

load_stop() {
  [[ -n "$LOAD_PID" ]] || return 0
  kill -TERM "$LOAD_PID" 2>/dev/null || true
  wait "$LOAD_PID" 2>/dev/null || true
  LOAD_PID=""
}

acked_count() { [[ -s "$1/acked.txt" ]] && wc -l < "$1/acked.txt" || echo 0; }

# ---- assertions ------------------------------------------------------------------------
check_no_loss() { # check_no_loss <dir> <port> <where>: every acknowledged id is there
  # A standby takes no temp table, so the ids travel as one array literal on
  # stdin — plus one random id that cannot exist, so a check that finds
  # nothing missing has provably looked.
  local dir=$1 port=$2 where=$3 missing
  [[ -s "$dir/acked.txt" ]] || return 0
  missing=$(PGPASSWORD=$(pgpass) "$PSQL" -X -qtA -v ON_ERROR_STOP=1 -h 127.0.0.1 -p "$port" -U payment -d payment <<SQL
SELECT count(*) - 1 FROM unnest('{$(cut -f1 "$dir/acked.txt" | paste -sd,),$(cat /proc/sys/kernel/random/uuid)}'::uuid[]) AS a(id)
WHERE NOT EXISTS (SELECT 1 FROM transactions t WHERE t.id = a.id)
   OR NOT EXISTS (SELECT 1 FROM entries e WHERE e.transaction_id = a.id);
SQL
) || { violation "$SCEN: cannot check acknowledged ids on $where"; return; }
  if [[ "$missing" -lt 0 ]]; then
    violation "$SCEN: the loss check on $where did not see its canary id missing"
  elif [[ "$missing" != 0 ]]; then
    violation "$SCEN: DATA LOSS — $missing acknowledged transfer(s) missing on $where"
  fi
}

caught_up() { # caught_up <node> <lsn>
  [[ "$(sql_node "$1" "SELECT pg_last_wal_replay_lsn() >= '$2'::pg_lsn")" == t ]]
}

check_ledger() {
  local imbalance mismatches
  imbalance=$(sql_rw "SELECT coalesce(string_agg(currency || '=' || total, ', '), '')
                      FROM (SELECT a.currency, SUM(b.raw_minor) AS total FROM balances b
                            JOIN accounts a ON a.id = b.account_id GROUP BY a.currency
                            HAVING SUM(b.raw_minor) <> 0) x")
  [[ -z "$imbalance" ]] || violation "$SCEN: conservation broken: $imbalance"
  mismatches=$(sql_rw "SELECT count(*) FROM balances b
                       LEFT JOIN (SELECT account_id, SUM(CASE direction WHEN 'credit' THEN amount_minor
                                                                        ELSE -amount_minor END) AS derived
                                  FROM entries GROUP BY account_id) d ON d.account_id = b.account_id
                       WHERE b.raw_minor <> COALESCE(d.derived, 0)")
  [[ "$mismatches" == 0 ]] || violation "$SCEN: $mismatches balance(s) disagree with their entries"
}

# The workers' leader holds session advisory lock 0x5041594D57524B31 on the primary.
worker_leaders() {
  sql_rw "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND granted
          AND classid = x'5041594D'::int::oid AND objid = x'57524B31'::int::oid AND objsubid = 1"
}

workers_recovered() { # workers_recovered <fault epoch ms>
  [[ "$(worker_leaders)" == 1 ]] || return 1
  [[ "$(sql_rw "SELECT count(*) FROM transactions WHERE sealed_seq IS NULL")" == 0 ]] || return 1
  [[ "$(sql_rw "SELECT count(*) FROM checkpoints WHERE created_at > to_timestamp($1 / 1000.0)")" -gt 0 ]] || return 1
  [[ "$(sql_rw "SELECT count(*) FROM outbox WHERE sent_at IS NULL")" == 0 ]] || return 1
}

rejoined() { # rejoined <old node> <new leader>
  local tl
  tl=$(api_get "$2" /patroni | jq -r .timeline)
  api_get "$1" /patroni | jq -e --argjson tl "$tl" \
    '.role == "replica" and .state == "running" and .replication_state == "streaming" and .timeline == $tl' >/dev/null
}

# A client connected straight to the thawed old leader (no HAProxy) tries to
# commit with a 5 s deadline; returns "acked", "refused" or "timeout".
# statement_timeout=0: a sync-replication wait can only end by replication,
# demotion or the client giving up (see the report on cancelled waits).
direct_write() { # direct_write <node> <statement_timeout ms>
  local out rc=0
  out=$(PGPASSWORD=$(pgpass) PGCONNECT_TIMEOUT=3 timeout 5 "$PSQL" -X -qtA -h 127.0.0.1 -p "545$1" -U payment -d payment \
          -c "SET statement_timeout = $2" -c "INSERT INTO ha_drill.probe (note) VALUES ('$SCEN direct write, statement_timeout=$2') RETURNING id" 2>&1) || rc=$?
  if [[ $rc -eq 0 ]]; then echo "acked:${out//$'\n'/ }"
  elif [[ $rc -eq 124 ]]; then echo timeout
  else echo "refused:$(tr '\n' ' ' <<< "$out" | cut -c1-120)"
  fi
}

# ---- one scenario ----------------------------------------------------------------------
RESULTS=()
run_scenario() { # run_scenario <round> <scenario>
  local round=$1 scen=$2 dir old new="" cand t0 t_leader tl split=0 m
  SCEN="r$round/$scen"
  dir=$OUT/r$round-$scen
  rm -rf "$dir"; mkdir -p "$dir"
  old=$(leader) || { violation "$SCEN: no leader before the fault"; return 1; }
  say "$SCEN: leader is pg-$old; $(for n in "${NODES[@]}"; do printf 'pg-%s=%s  ' "$n" "$(member_state "$n")"; done)"
  load_start "$dir"
  sleep "$WARMUP"
  [[ "$(acked_count "$dir")" -gt 0 ]] || { violation "$SCEN: no transfer acknowledged during warm-up"; load_stop; return 1; }

  cand=""
  for m in "${NODES[@]}"; do
    if [[ $m != "$old" ]] && quorum_member "$m"; then cand=$m; break; fi
  done
  [[ -n "$cand" ]] || { violation "$SCEN: no quorum standby before the fault"; load_stop; return 1; }
  local t_kill="" s1 s2
  t0=$(now_ms)
  case "$scen" in
    crash) drv_node kill "$old" ;;
    freeze) drv_node freeze "$old" ;;
    quorum-loss)
      # Both standbys are cut off (longer than the app's statement timeout),
      # then the leader dies before they come back. A frozen process's kernel
      # still buffers what is sent to it, so the leader's walsenders are
      # terminated too: from here on no WAL reaches a standby, as in a real
      # partition.
      read -r s1 s2 <<< "$(for m in "${NODES[@]}"; do [[ $m == "$old" ]] || printf '%s ' "$m"; done)"
      drv_node freeze "$s1"; drv_node freeze "$s2"
      sql_node "$old" "SELECT count(pg_terminate_backend(pid)) FROM pg_stat_replication" >/dev/null || true
      sleep "$QL_HANG"
      t_kill=$(now_ms)
      drv_node kill "$old"
      drv_node thaw "$s1"; drv_node thaw "$s2"
      # Patroni dropped both from the quorum when they vanished, so neither
      # may be promoted (either could lack an acknowledged commit): the
      # cluster must stay leaderless until the old leader is back.
      local until=$(( $(now_ms) + 25000 ))
      while (( $(now_ms) < until )); do
        if is_primary "$s1" || is_primary "$s2"; then
          violation "$SCEN: a standby cut off from the quorum was promoted"; break
        fi
        sleep 0.5
      done
      drv_node start "$old" ;;
    switchover) drv_patronictl "$cand" switchover --leader "pg-$old" --candidate "pg-$cand" --force >"$dir/switchover.log" 2>&1 \
                  || violation "$SCEN: patronictl switchover failed: $(tail -n2 "$dir/switchover.log")" ;;
  esac
  info "fault injected at $t0"

  # A new leader, polled every 0.1 s — and never two at once (a frozen old
  # leader cannot answer, so it is only asked in the other scenarios). After
  # quorum-loss the returning old leader is the only legitimate one.
  local deadline=$((t0 + LEADER_TIMEOUT * 1000)) found
  while (( $(now_ms) < deadline )); do
    found=()
    for m in "${NODES[@]}"; do
      [[ $m == "$old" && $scen == freeze ]] && continue
      API_TIMEOUT=0.5 is_primary "$m" && found+=("$m")
    done
    [[ ${#found[@]} -le 1 ]] || split=1
    for m in "${found[@]}"; do [[ $m != "$old" || $scen == quorum-loss ]] && { new=$m; break 2; }; done
    sleep 0.1
  done
  t_leader=$(secs_since "$t0")
  [[ $split -eq 0 ]] || violation "$SCEN: two members answered /primary at the same time"
  if [[ -z "$new" ]]; then
    violation "$SCEN: no new leader within ${LEADER_TIMEOUT}s"
    load_stop; return 1
  fi
  [[ $scen != switchover || $new == "$cand" ]] || violation "$SCEN: switchover went to pg-$new, not pg-$cand"
  info "new leader pg-$new after ${t_leader}s"
  wait_until 30 curl -fsS http://127.0.0.1:7450/health || violation "$SCEN: HAProxy routes no primary 30 s after the new leader"

  # Bring the old leader back.
  local probe="" probe_app="" t_back
  case "$scen" in
    crash) sleep 2; drv_node start "$old" ;;
    freeze)
      # Both probes start the moment the old leader runs again, before
      # Patroni has had a chance to demote it.
      drv_node thaw "$old"
      direct_write "$old" 0 > "$dir/probe.txt" &
      local probe_pid=$!
      direct_write "$old" 2000 > "$dir/probe-app-like.txt"
      wait "$probe_pid" || true
      probe=$(cat "$dir/probe.txt"); probe_app=$(cat "$dir/probe-app-like.txt")
      ;;
  esac
  t_back=$(now_ms)
  if [[ $scen == freeze ]]; then
    info "direct commit on the thawed old leader (statement_timeout=0):    $probe"
    info "direct commit on the thawed old leader (statement_timeout=2s):   $probe_app"
    [[ $probe != acked* ]] || violation "$SCEN: SPLIT BRAIN — the thawed old leader acknowledged a direct write ($probe)"
  fi

  for m in "${NODES[@]}"; do
    [[ $m == "$new" ]] && continue
    if wait_until "$REJOIN_TIMEOUT" rejoined "$m" "$new"; then
      [[ $m != "$old" ]] || info "pg-$old rejoined as a streaming replica after $(secs_since "$t_back")s"
    else
      violation "$SCEN: pg-$m is not a streaming replica of pg-$new ${REJOIN_TIMEOUT}s after recovery ($(member_state "$m"))"
    fi
  done
  [[ "$(primaries)" == 1 ]] || violation "$SCEN: $(primaries) members answer /primary after recovery"
  if [[ $old != "$new" ]]; then
    local ro
    ro=$(sql_node "$old" "INSERT INTO ha_drill.probe (note) VALUES ('$SCEN after rejoin')" 2>&1 || true)
    [[ "$ro" == *"read-only transaction"* ]] || violation "$SCEN: demoted pg-$old did not refuse a write: ${ro:-accepted}"
  fi

  sleep "$SETTLE"
  load_stop
  local acked lsn
  acked=$(acked_count "$dir")
  if [[ $scen == quorum-loss ]]; then
    # With synchronous_mode_strict nothing can be acknowledged while both
    # standbys hang (a second of grace for commits already confirmed).
    local blind
    blind=$(awk -v a="$((t0 + 1000))" -v b="$t_kill" '$2 > a && $2 <= b' "$dir/acked.txt" | wc -l)
    info "transfers acknowledged while both standbys hung: $blind"
    [[ "$blind" == 0 ]] || violation "$SCEN: $blind transfer(s) acknowledged while no standby could confirm them"
  fi
  m=$(python3 -I "$ROOT/scripts/ha-load.py" analyze --out "$dir" --fault-ms "$t0")
  echo "$m" > "$dir/metrics.json"

  # Every acknowledged transfer on every member, once each has replayed up to now.
  lsn=$(sql_rw "SELECT pg_current_wal_lsn()")
  for n in "${NODES[@]}"; do
    if [[ $n != "$new" ]] && ! wait_until 60 caught_up "$n" "$lsn"; then
      violation "$SCEN: pg-$n did not replay up to $lsn within 60 s"; continue
    fi
    check_no_loss "$dir" "545$n" "pg-$n"
  done
  [[ "$(sql_rw "SELECT count(*) FROM ha_drill.probe WHERE note LIKE '$SCEN direct write%'")" == 0 ]] \
    || violation "$SCEN: a write made on the old leader after the fault reached the new leader"
  check_ledger
  wait_until 90 workers_recovered "$t0" \
    || violation "$SCEN: workers did not recover within 90 s (leader locks=$(worker_leaders), unsealed=$(sql_rw "SELECT count(*) FROM transactions WHERE sealed_seq IS NULL"), unsent outbox=$(sql_rw "SELECT count(*) FROM outbox WHERE sent_at IS NULL"))"
  local chain
  chain=$(verify_chain) || true
  [[ "$(jq -r .status <<< "$chain" 2>/dev/null)" == intact ]] || violation "$SCEN: verify-chain: ${chain:-no output}"
  # WAL archiving continues from the new leader, on its new timeline (check
  # forces a segment switch and waits until that segment is in the repository).
  drv_pgbackrest "$new" check >"$dir/archive-check.log" 2>&1 \
    || violation "$SCEN: WAL archiving fails on the new leader pg-$new: $(tail -n2 "$dir/archive-check.log" | tr '\n' ' ')"
  # ... and the promotion started a base backup on the new timeline (ha-on-role-change).
  tl=$(api_get "$new" /patroni | jq -r .timeline)
  wait_until 120 timeline_backup "$new" "$tl" \
    || violation "$SCEN: no pgBackRest backup on the new timeline $tl after the promotion (ha-on-role-change)"

  info "acked=$acked tl=$tl chain=$(jq -c '{checkpoints_verified, transactions_covered}' <<< "$chain" 2>/dev/null) client=$m"
  RESULTS+=("$(jq -c --arg s "$scen" --argjson r "$round" --arg from "pg-$old" --arg to "pg-$new" \
    --argjson leader "$t_leader" --argjson acked "$acked" --arg probe "${probe_app:-}" \
    '{scenario: $s, round: $r, from: $from, to: $to, new_leader_s: $leader, acked: $acked,
      app_like_direct_write: $probe} + .' <<< "$m")")
}

quorum_member() { api_get "$1" /patroni | jq -e '.quorum_standby == true or .sync_standby == true' >/dev/null; }

pitr_check() {
  say "point-in-time recovery from the WAL archive alone"
  local l dir
  SCEN=pitr
  l=$(leader) || { violation "pitr: no leader"; return; }
  drv_pgbackrest "$l" check >"$OUT/archive-final.log" 2>&1 \
    || { violation "pitr: final archive check on pg-$l failed"; return; }
  pitr_start
  if ! wait_until 300 pitr_promoted; then
    violation "pitr: the restored instance did not complete recovery within 300 s (see $OUT/pitr.log)"
    pitr_stop; return
  fi
  for dir in "$OUT"/r*-*/; do check_no_loss "${dir%/}" 5459 "the instance restored from the archive"; done
  info "restored to timeline $(sql_port 5459 "SELECT timeline_id FROM pg_control_checkpoint()") with $(sql_port 5459 "SELECT count(*) FROM transactions") transactions: every acknowledged transfer of every scenario is there"
  pitr_stop
}

summary() {
  say "summary ($MODE mode, rps=$RPS)"
  printf '%s\n' "${RESULTS[@]}" > "$OUT/results.jsonl"
  printf '%-11s %5s %-11s %9s %9s %9s %7s %7s  %s\n' scenario round from-\>to leader_s outage_s errwin_s errors acked statuses
  jq -r '[.scenario, .round, (.from + "->" + .to), .new_leader_s, .write_outage_s, .error_window_s,
          .errors_after_fault, .acked, (.statuses_after_fault | to_entries | map("\(.key)=\(.value)") | join(","))]
         | @tsv' "$OUT/results.jsonl" | while IFS=$'\t' read -r a b c d e f g h i; do
    printf '%-11s %5s %-11s %9s %9s %9s %7s %7s  %s\n' "$a" "$b" "$c" "$d" "$e" "$f" "$g" "$h" "$i"
  done
  jq -rs 'group_by(.scenario)[] | . as $g
    | def med(f): ([$g[] | f] | sort | .[(length - 1) / 2 | floor]);
    "\($g[0].scenario): n=\($g | length) new leader median \(med(.new_leader_s))s (min \([$g[].new_leader_s] | min), max \([$g[].new_leader_s] | max)); write outage median \(med(.write_outage_s))s (max \([$g[].write_outage_s] | max)); errors median \(med(.errors_after_fault))"' \
    "$OUT/results.jsonl"
}

cleanup() {
  local rc=$?
  load_stop
  pitr_stop
  mkdir -p "$OUT/logs"
  if [[ $MODE == local ]]; then cp -r "$W/log" "$W/app" "$OUT/logs/" 2>/dev/null || true
  else "${COMPOSE[@]}" logs --no-color --timestamps > "$OUT/logs/compose.log" 2>&1 || true
  fi
  if [[ $KEEP -eq 0 ]]; then drv_down; else app_down; echo "kept: $W (scripts/ha-local.sh down / docker compose down)"; fi
  exit "$rc"
}

# ---- main ------------------------------------------------------------------------------
for t in curl jq openssl python3 "$PSQL"; do command -v "$t" >/dev/null || die "$t is required"; done
[[ $MODE == compose ]] || [[ $EUID -eq 0 ]] || die "local mode runs as root (Postgres runs as postgres)"
rm -rf "$OUT"; mkdir -p "$OUT"
trap cleanup EXIT
say "cluster ($MODE mode)"
drv_up
app_up
setup_data
say "base backup (pgBackRest, on the leader)"
l=$(leader) || die "no leader"
drv_pgbackrest "$l" --type=full backup >"$OUT/backup.log" 2>&1 || die "base backup failed (see $OUT/backup.log)"
# PITR restores THIS backup (not the newest, which a promotion adds), so
# recovery has to replay the archive across every failover below.
BASE_SET=$(drv_pgbackrest "$l" info --output=json | jq -r '.[0].backup[-1].label')
info "base backup $BASE_SET"
for round in $(seq 1 "$ROUNDS"); do
  for scen in "${SCENARIO_LIST[@]}"; do
    run_scenario "$round" "$scen" || true
  done
done
if [[ $MODE == compose ]]; then
  # Docker restarts nothing whose dependencies are unhealthy: every healthcheck
  # must still pass after all the faults (etcd's, too, with auth on).
  bad=$(docker ps --filter "label=com.docker.compose.project=$DRILL_PROJECT" --filter health=unhealthy --format '{{.Names}}')
  [[ -z "$bad" ]] || violation "unhealthy containers after the scenarios: $(tr '\n' ' ' <<< "$bad")"
fi
pitr_check
summary
if [[ ${#VIOLATIONS[@]} -gt 0 ]]; then
  say "FAIL: ${#VIOLATIONS[@]} violation(s)"
  printf '    %s\n' "${VIOLATIONS[@]}"
  exit 1
fi
say "PASS: ${#RESULTS[@]} scenario run(s), zero acknowledged transfers lost"
