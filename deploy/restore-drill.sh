#!/usr/bin/env bash
# Restore drill: prove the latest backup is restorable AND the ledger inside it
# is internally consistent. Run it monthly (host cron) and after any change to
# the backup sidecar. Exit 0 = PASS, non-zero = FAIL. Nothing in the live stack
# is touched: the dump is restored into a throwaway container on tmpfs.
#
#   ./restore-drill.sh                     newest dump from the `backups` volume
#   ./restore-drill.sh /path/to/dump.sql.gz[.gpg]   a specific dump (e.g. one
#                                          copied back from off-box storage;
#                                          .gpg is decrypted with your key)
#
# Checks (the same invariants payment-workers reconciles continuously):
#   1. conservation  — per currency, SUM(balances.raw_minor) = 0
#   2. integrity     — every balance equals the signed sum of its entries
#   3. shape         — the migration table, transactions and entries are present
set -euo pipefail
cd "$(dirname "$0")"

PROJECT=${COMPOSE_PROJECT_NAME:-payment}
PG_IMAGE=${PG_IMAGE:-postgres:16.15}
NAME="payment-restore-drill-$$"
DUMP_ARG=${1:-}

cleanup() { docker rm -f "$NAME" >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo "==> starting scratch Postgres ($PG_IMAGE) on tmpfs..."
docker run -d --name "$NAME" \
  --tmpfs /var/lib/postgresql/data:rw,size=2g \
  -v "${PROJECT}_backups:/backups:ro" \
  -e POSTGRES_USER=payment -e POSTGRES_PASSWORD=drill -e POSTGRES_DB=payment \
  "$PG_IMAGE" >/dev/null
for _ in $(seq 1 60); do
  docker exec "$NAME" pg_isready -U payment -d payment >/dev/null 2>&1 && break
  sleep 1
done
docker exec "$NAME" pg_isready -U payment -d payment >/dev/null || { echo "FAIL: scratch postgres did not start"; exit 1; }

# ---- locate the dump ---------------------------------------------------------
if [[ -n "$DUMP_ARG" ]]; then
  [[ -f "$DUMP_ARG" ]] || { echo "FAIL: no such file $DUMP_ARG"; exit 1; }
  local_dump="$DUMP_ARG"
  if [[ "$local_dump" == *.gpg ]]; then
    echo "==> decrypting $(basename "$local_dump") with gpg..."
    tmp=$(mktemp); trap 'rm -f "$tmp"; cleanup' EXIT
    gpg --quiet --decrypt "$local_dump" > "$tmp"
    local_dump="$tmp"
  fi
  docker cp "$local_dump" "$NAME:/tmp/dump.sql.gz"
  dump_desc="$DUMP_ARG"
else
  # prodrigestivill/postgres-backup-local keeps the newest dump under last/
  # (real files, timestamped); the `*-latest` entries are symlinks whose mtime
  # would win an `ls -t`, so they are skipped.
  dump_in=$(docker exec "$NAME" sh -c 'ls -t /backups/last/*.sql.gz /backups/daily/*.sql.gz 2>/dev/null | grep -v -- "-latest" | head -n1' || true)
  [[ -n "$dump_in" ]] || { echo "FAIL: no dump found in the ${PROJECT}_backups volume (has postgres-backup run yet?)"; exit 1; }
  docker exec "$NAME" cp "$dump_in" /tmp/dump.sql.gz
  dump_desc="$dump_in"
fi
size=$(docker exec "$NAME" sh -c 'du -h /tmp/dump.sql.gz | cut -f1')
echo "==> restoring $dump_desc ($size)..."
docker exec "$NAME" sh -c 'gunzip -c /tmp/dump.sql.gz | psql -q -v ON_ERROR_STOP=1 -U payment -d payment >/dev/null' \
  || { echo "FAIL: restore reported errors"; exit 1; }

q() { docker exec "$NAME" psql -tA -v ON_ERROR_STOP=1 -U payment -d payment -c "$1"; }

# ---- 3. shape ------------------------------------------------------------------
if [[ "$(q "SELECT to_regclass('public.transactions') IS NOT NULL")" != "t" ]]; then
  echo "FAIL: the dump contains no ledger tables — was it taken before the server applied migrations?"
  exit 1
fi
migrations=$(q "SELECT COUNT(*) FROM _sqlx_migrations")
txns=$(q "SELECT COUNT(*) FROM transactions")
entries=$(q "SELECT COUNT(*) FROM entries")
accounts=$(q "SELECT COUNT(*) FROM balances")
checkpoints=$(q "SELECT COUNT(*) FROM checkpoints")
echo "    migrations=$migrations transactions=$txns entries=$entries accounts=$accounts checkpoints=$checkpoints"
fail=0
[[ "$migrations" -ge 21 ]] || { echo "FAIL: only $migrations migrations applied in the dump"; fail=1; }

# ---- 1. conservation -------------------------------------------------------------
imbalance=$(q "SELECT string_agg(currency || '=' || total, ', ')
               FROM (SELECT a.currency, SUM(b.raw_minor) AS total
                     FROM balances b JOIN accounts a ON a.id = b.account_id
                     GROUP BY a.currency HAVING SUM(b.raw_minor) <> 0) x")
if [[ -n "$imbalance" ]]; then echo "FAIL: conservation broken: $imbalance"; fail=1; else echo "    conservation: OK (every currency sums to 0)"; fi

# ---- 2. integrity ------------------------------------------------------------------
mismatches=$(q "SELECT COUNT(*) FROM balances b
                LEFT JOIN (SELECT account_id,
                                  SUM(CASE direction WHEN 'credit' THEN amount_minor ELSE -amount_minor END) AS derived
                           FROM entries GROUP BY account_id) d ON d.account_id = b.account_id
                WHERE b.raw_minor <> COALESCE(d.derived, 0)")
if [[ "$mismatches" != "0" ]]; then echo "FAIL: $mismatches account balance(s) disagree with their entries"; fail=1; else echo "    integrity: OK (all $accounts balances match their entries)"; fi

if [[ $fail -eq 0 ]]; then
  echo "PASS: $dump_desc restores cleanly and the ledger is consistent"
  exit 0
fi
echo "FAIL: see above"
exit 1
