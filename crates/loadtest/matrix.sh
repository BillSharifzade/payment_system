#!/usr/bin/env bash
# Interleaved A/B runs of payment-loadtest, each on a freshly recreated database, then one
# median/min–max table (payment-loadtest summarize). The variant order alternates between
# repeats so slow drift on a shared box does not favour either side.
#
#   ADMIN_URL=postgres://postgres@127.0.0.1:5442/postgres \
#   DB_URL=postgres://payment:payment_dev_pw@127.0.0.1:5442/payment_perf \
#   REPEATS=3 OUT=/tmp/bench \
#   crates/loadtest/matrix.sh base=/bin/a/payment-server:/bin/a/payment-loadtest \
#                             new=/bin/b/payment-server:/bin/b/payment-loadtest
#
# A variant is NAME=SERVER_BIN:LOADTEST_BIN (http cases spawn SERVER_BIN; direct cases run
# LOADTEST_BIN, which links the storage/api code under test). CASES is one case per line,
# "name|harness flags"; the default is the README matrix. ADMIN_URL must be able to drop and
# create the database named in DB_URL — use a disposable cluster.
set -euo pipefail

: "${ADMIN_URL:?superuser URL used to recreate the benchmark database}"
: "${DB_URL:?database URL the server and harness use}"
REPEATS=${REPEATS:-3}
OUT=${OUT:-./bench-$(date +%Y%m%d-%H%M%S)}
COMMON=${COMMON:---users 1000 --concurrency 64 --duration 30s --warmup 5s --pg-stats}
CASES=${CASES:-"http-uniform|--workload uniform
http-hot|--workload hot --merchants 10
http-contention|--workload contention --concurrency 16
http-mixed|--workload mixed
http-signed|--workload uniform --sign
http-fx-check|--mix transfer=2,fx=1,check=1
direct-uniform|--mode direct --workload uniform
direct-hot|--mode direct --workload hot --merchants 10
direct-contention|--mode direct --workload contention --concurrency 16"}

(($# >= 1)) || { echo "usage: $0 NAME=SERVER_BIN:LOADTEST_BIN..." >&2; exit 2; }
db=${DB_URL##*/}; db=${db%%\?*}
[[ "$db" =~ ^[a-z_][a-z0-9_]*$ ]] || { echo "bad database name in DB_URL: $db" >&2; exit 2; }
owner=${DB_URL#*://}; owner=${owner%%[:@]*}
mkdir -p "$OUT"

fresh_db() {
  psql -X -q -v ON_ERROR_STOP=1 "$ADMIN_URL" \
    -c "DROP DATABASE IF EXISTS $db WITH (FORCE)" -c "CREATE DATABASE $db OWNER $owner"
  # Per-statement timings when the cluster preloads pg_stat_statements; harmless otherwise.
  psql -X -q "${ADMIN_URL%/*}/$db" -c "CREATE EXTENSION IF NOT EXISTS pg_stat_statements" \
    >/dev/null 2>&1 || true
}

variants=("$@")
reports=()
for rep in $(seq 1 "$REPEATS"); do
  order=("${variants[@]}")
  if ((rep % 2 == 0)); then
    order=(); for ((i = ${#variants[@]} - 1; i >= 0; i--)); do order+=("${variants[i]}"); done
  fi
  while IFS='|' read -r name flags; do
    [[ -n "$name" ]] || continue
    for v in "${order[@]}"; do
      vname=${v%%=*}; bins=${v#*=}; server=${bins%%:*}; harness=${bins#*:}
      report="$OUT/$name.$vname.$rep.json"
      fresh_db
      echo "==> [$rep/$REPEATS] $name / $vname"
      mode_flags=(--server-bin "$server" --server-log "$OUT/$name.$vname.$rep.server.log")
      [[ " $flags " == *" --mode direct "* ]] && mode_flags=()
      # shellcheck disable=SC2086 # COMMON and flags are word lists by design
      "$harness" --database-url "$DB_URL" $COMMON $flags "${mode_flags[@]}" \
        --label "$name/$vname" --report "$report" --allow-errors 2>"$OUT/$name.$vname.$rep.log" \
        || echo "    run failed (exit $?): see $OUT/$name.$vname.$rep.log"
      [[ -f "$report" ]] && reports+=("$report")
      tail -n 4 "$OUT/$name.$vname.$rep.log" | sed 's/^/    /'
    done
  done <<<"$CASES"
done

harness=${variants[0]#*=}; harness=${harness#*:}
"$harness" summarize "${reports[@]}" | tee "$OUT/summary.md"
