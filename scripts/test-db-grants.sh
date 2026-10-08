#!/usr/bin/env bash
# Prove the least-privilege role model end to end on a scratch database:
#   1. deploy/postgres/10-roles.sh creates payment_owner / payment_app /
#      payment_backup / payment_monitor (passwords from throwaway files);
#   2. every migration is applied AS payment_owner, the way payment-server does
#      it with MIGRATION_DATABASE_URL (sqlx bookkeeping table included);
#   3. AS payment_app: ledger/audit UPDATE/DELETE, DDL, TRIGGER games and
#      migration-table writes all fail with "permission denied", while the DML
#      the app really performs succeeds;
#   4. a table created by a later migration gets the default DML grants.
#
#   scripts/test-db-grants.sh <superuser-url> <scratch-db> [migrations-dir]
#
# Example (CI):  scripts/test-db-grants.sh postgres://payment:payment_dev_pw@localhost:5432/postgres payment_grants
# The scratch database is DROPPED and recreated. The four roles are created at
# CLUSTER level if missing (and their passwords reset) — use a disposable
# cluster or one where that is acceptable. Needs psql >= 15.
set -euo pipefail

admin_url=${1:?superuser connection URL (to any database)}
db=${2:?scratch database name (will be dropped and recreated)}
here=$(cd "$(dirname "$0")/.." && pwd)
migrations=${3:-$here/migrations}

[[ "$db" =~ ^[a-z_][a-z0-9_]*$ ]] || { echo "bad database name: $db" >&2; exit 2; }

# Split the URL so the same host/port/superuser can be reused with other roles.
proto_stripped=${admin_url#*://}
userinfo=${proto_stripped%%@*}
hostpart=${proto_stripped#*@}; hostpart=${hostpart%%/*}
su_user=${userinfo%%:*}
su_pass=${userinfo#*:}
pg_host=${hostpart%%:*}
pg_port=${hostpart##*:}; [[ "$pg_port" == "$hostpart" ]] && pg_port=5432
export PGHOST="$pg_host" PGPORT="$pg_port" PGCONNECT_TIMEOUT=10

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
chmod 700 "$work"
for s in pg_owner_password pg_app_password pg_backup_password pg_monitor_password; do
  # (od, not openssl: this also runs inside the postgres image in CI)
  (umask 077; head -c 24 /dev/urandom | od -An -tx1 -v | tr -d ' \n' > "$work/$s")
done
owner_pw=$(cat "$work/pg_owner_password")
app_pw=$(cat "$work/pg_app_password")

as_super() { PGPASSWORD="$su_pass" psql -X -q -v ON_ERROR_STOP=1 -U "$su_user" "$@"; }
as_owner() { PGPASSWORD="$owner_pw" psql -X -q -v ON_ERROR_STOP=1 -U payment_owner -d "$db" "$@"; }
as_app()   { PGPASSWORD="$app_pw"   psql -X -q -v ON_ERROR_STOP=1 -U payment_app   -d "$db" "$@"; }

echo "==> recreating scratch database $db on $pg_host:$pg_port"
as_super -d postgres -c "DROP DATABASE IF EXISTS $db WITH (FORCE)" -c "CREATE DATABASE $db"

echo "==> deploy/postgres/10-roles.sh"
SECRETS_DIR="$work" POSTGRES_DB="$db" POSTGRES_USER="$su_user" PGPASSWORD="$su_pass" \
  bash "$here/deploy/postgres/10-roles.sh"

echo "==> applying migrations from $migrations as payment_owner"
as_owner -c "CREATE TABLE IF NOT EXISTS _sqlx_migrations (
    version BIGINT PRIMARY KEY, description TEXT NOT NULL,
    installed_on TIMESTAMPTZ NOT NULL DEFAULT now(), success BOOLEAN NOT NULL,
    checksum BYTEA NOT NULL, execution_time BIGINT NOT NULL)"
applied=0
for f in "$migrations"/[0-9]*.sql; do
  name=$(basename "$f" .sql)
  version=$((10#${name%%_*}))
  if head -n1 "$f" | grep -q -- '-- no-transaction'; then
    as_owner -f "$f"
  else
    as_owner -1 -f "$f"
  fi
  as_owner -c "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
               VALUES ($version, '${name#*_}', true, '\\x00', 0)"
  applied=$((applied + 1))
done
echo "    $applied migrations applied"

pass=0; fail=0
ok()  { echo "    ok    $1"; pass=$((pass + 1)); }
bad() { echo "    FAIL  $1"; fail=$((fail + 1)); }

# expect_denied <description> <sql>: must fail with a privilege error.
expect_denied() {
  local out
  if out=$(as_app -c "$2" 2>&1); then
    bad "$1 — was ALLOWED"
  elif grep -qiE 'permission denied|must be owner|must be superuser|insufficient privilege' <<<"$out"; then
    ok "$1 (denied)"
  else
    bad "$1 — failed for another reason: $(head -n1 <<<"$out")"
  fi
}

echo "==> payment_app: forbidden statements"
for t in entries checkpoints admin_actions screening_events biometric_events voided_transactions \
         checkpoint_anchors tb_intents; do
  if [[ "$(as_super -d "$db" -tAc "SELECT to_regclass('public.$t') IS NOT NULL")" != t ]]; then
    # Later migrations add these (0023, 0031, 0033); the others must exist.
    case $t in voided_transactions|checkpoint_anchors|tb_intents)
      echo "    skip  $t (not in this schema)"; continue ;; esac
    bad "table $t missing"; continue
  fi
  expect_denied "UPDATE $t"   "UPDATE $t SET created_at = created_at"
  expect_denied "DELETE $t"   "DELETE FROM $t"
  expect_denied "TRUNCATE $t" "TRUNCATE $t"
done
expect_denied "DELETE transactions"            "DELETE FROM transactions"
expect_denied "UPDATE transactions.created_at" "UPDATE transactions SET created_at = now()"
expect_denied "UPDATE transactions.id"         "UPDATE transactions SET id = id"
expect_denied "UPDATE transactions.seq"        "UPDATE transactions SET seq = seq"
expect_denied "TRUNCATE transactions"          "TRUNCATE transactions"
expect_denied "disable triggers on entries"    "ALTER TABLE entries DISABLE TRIGGER ALL"
expect_denied "session_replication_role"       "SET session_replication_role = replica"
expect_denied "DROP TABLE entries"             "DROP TABLE entries"
expect_denied "CREATE TABLE in public"         "CREATE TABLE rogue (id int)"
expect_denied "CREATE TRIGGER on entries"      "CREATE TRIGGER t BEFORE INSERT ON entries FOR EACH ROW EXECUTE FUNCTION suppress_redundant_updates_trigger()"
expect_denied "INSERT _sqlx_migrations"        "INSERT INTO _sqlx_migrations VALUES (999999, 'x', now(), true, '\\x00', 0)"
expect_denied "UPDATE _sqlx_migrations"        "UPDATE _sqlx_migrations SET success = false"
expect_denied "DELETE _sqlx_migrations"        "DELETE FROM _sqlx_migrations"
if [[ "$(as_super -d "$db" -tAc "SELECT to_regclass('public.reconcile_watermark') IS NOT NULL")" == t ]]; then
  expect_denied "DELETE reconciled_sums"       "DELETE FROM reconciled_sums"
  expect_denied "INSERT reconcile_watermark"   "INSERT INTO reconcile_watermark (id, through_sealed_seq) VALUES (true, 0)"
  expect_denied "DELETE reconcile_watermark"   "DELETE FROM reconcile_watermark"
fi
expect_denied "ALTER ROLE payment_app"         "ALTER ROLE payment_app SUPERUSER"
expect_denied "GRANT pg_write_server_files"    "GRANT pg_write_server_files TO payment_app"

echo "==> payment_app: the DML the app performs (one transaction, rolled back)"
# A balanced posting between two seeded system accounts, then everything the
# sealer, relay, retention, auth and admin paths do. Rolled back at the end, so
# deferred integrity triggers (if any) never fire.
if out=$(as_app 2>&1 <<'SQL'
BEGIN;
SELECT count(*) FROM _sqlx_migrations;
SELECT count(*) FROM accounts;
INSERT INTO transactions (id) VALUES ('0192f0c0-0000-7000-8000-00000000a001');
INSERT INTO entries (id, transaction_id, account_id, direction, amount_minor, currency) VALUES
  ('0192f0c0-0000-7000-8000-00000000b001', '0192f0c0-0000-7000-8000-00000000a001',
   '00000000-0000-0000-0000-000000001000', 'debit', 100, 'TJS'),
  ('0192f0c0-0000-7000-8000-00000000b002', '0192f0c0-0000-7000-8000-00000000a001',
   '00000000-0000-0000-0000-000000001001', 'credit', 100, 'TJS');
UPDATE balances SET raw_minor = raw_minor - 100, version = version + 1
 WHERE account_id = '00000000-0000-0000-0000-000000001000';
UPDATE balances SET raw_minor = raw_minor + 100, version = version + 1
 WHERE account_id = '00000000-0000-0000-0000-000000001001';
SELECT account_id FROM balances WHERE account_id = '00000000-0000-0000-0000-000000001000' FOR UPDATE;
UPDATE transactions SET sealed_seq = 9000000001
 WHERE id = '0192f0c0-0000-7000-8000-00000000a001' AND sealed_seq IS NULL;
INSERT INTO checkpoints (id, seq, from_txn_seq, to_txn_seq, txn_count, merkle_root,
                         prev_checkpoint_hash, checkpoint_hash, signature, public_key)
VALUES ('0192f0c0-0000-7000-8000-00000000c001', 9000000001, 1, 1, 1, '\x00', '\x00', '\x00', '\x00', '\x00');
INSERT INTO outbox (id, aggregate_id, event_type, payload)
VALUES ('0192f0c0-0000-7000-8000-00000000d001', '0192f0c0-0000-7000-8000-00000000a001', 'transaction.posted', '{}');
SELECT id FROM outbox WHERE sent_at IS NULL FOR UPDATE SKIP LOCKED;
UPDATE outbox SET sent_at = now() WHERE id = '0192f0c0-0000-7000-8000-00000000d001';
DELETE FROM outbox WHERE sent_at < now() - interval '7 days';
DELETE FROM idempotency_keys WHERE created_at < now() - interval '30 days';
DELETE FROM refresh_tokens WHERE expires_at < now() - interval '30 days';
DELETE FROM kyc_documents WHERE created_at < now() - interval '3650 days';
INSERT INTO users (id, phone, password_hash) VALUES ('0192f0c0-0000-7000-8000-00000000e001', '992000000001', 'x');
UPDATE users SET kyc_level = 1 WHERE id = '0192f0c0-0000-7000-8000-00000000e001';
SELECT status FROM users WHERE id = '0192f0c0-0000-7000-8000-00000000e001' FOR UPDATE;
INSERT INTO admin_actions (id, admin_id, action, target)
VALUES ('0192f0c0-0000-7000-8000-00000000f001', '0192f0c0-0000-7000-8000-00000000e001', 'deposit', 'x');
INSERT INTO screening_events (id, amount_minor, currency, decision)
VALUES ('0192f0c0-0000-7000-8000-00000000f002', 1, 'TJS', 'allowed');
-- Tables of later migrations (0023/0024), when present: voids, deposit
-- requests (insert + decide), terminals (insert + revoke).
DO $$
BEGIN
  IF to_regclass('public.voided_transactions') IS NOT NULL THEN
    INSERT INTO transactions (id) VALUES ('0192f0c0-0000-7000-8000-00000000a002');
    EXECUTE $q$INSERT INTO voided_transactions (id, voided_by)
               VALUES ('0192f0c0-0000-7000-8000-00000000a002', '0192f0c0-0000-7000-8000-00000000e001')$q$;
  END IF;
  IF to_regclass('public.deposit_requests') IS NOT NULL THEN
    EXECUTE $q$INSERT INTO deposit_requests (id, user_account, amount_minor, currency, requested_by)
               VALUES ('0192f0c0-0000-7000-8000-00000000a003', '00000000-0000-0000-0000-000000001000', 5, 'TJS',
                       '0192f0c0-0000-7000-8000-00000000e001')$q$;
    EXECUTE $q$UPDATE deposit_requests SET status = 'rejected', decided_by = requested_by, decided_at = now()
               WHERE id = '0192f0c0-0000-7000-8000-00000000a003'$q$;
  END IF;
  IF to_regclass('public.reconcile_watermark') IS NOT NULL THEN
    -- what workers::reconcile does: lock + advance the watermark, upsert sums
    PERFORM through_sealed_seq FROM reconcile_watermark FOR UPDATE;
    EXECUTE $q$UPDATE reconcile_watermark SET through_sealed_seq = through_sealed_seq, updated_at = now()$q$;
    EXECUTE $q$INSERT INTO reconciled_sums (account_id, sum_minor)
               VALUES ('00000000-0000-0000-0000-000000001000', 1)
               ON CONFLICT (account_id) DO UPDATE
               SET sum_minor = reconciled_sums.sum_minor + EXCLUDED.sum_minor, updated_at = now()$q$;
  END IF;
  IF to_regclass('public.checkpoint_anchors') IS NOT NULL THEN
    -- what the anchoring leader does: one witness token for the checkpoint above
    EXECUTE $q$INSERT INTO checkpoint_anchors (id, checkpoint_seq, checkpoint_hash, kind, witness, status, proof, attested_at)
               VALUES ('0192f0c0-0000-7000-8000-00000000a005', 9000000001, decode(repeat('00', 32), 'hex'),
                       'rfc3161', 'https://tsa.example', 'complete', '\x01', now())$q$;
  END IF;
  IF to_regclass('public.tb_intents') IS NOT NULL THEN
    -- the TigerBeetle hybrid ledger's commit record and recovery watermark
    EXECUTE $q$INSERT INTO tb_intents (attempt_id, transaction_id, outcome)
               VALUES ('0192f0c0-0000-7000-8000-00000000a006', '0192f0c0-0000-7000-8000-00000000a001', 'commit')$q$;
    EXECUTE $q$INSERT INTO tb_recovery_watermark (cluster_id, through_timestamp)
               VALUES ('0192f0c0-0000-7000-8000-00000000a007', 0)
               ON CONFLICT (cluster_id) DO UPDATE SET through_timestamp = EXCLUDED.through_timestamp$q$;
  END IF;
  IF to_regclass('public.terminals') IS NOT NULL THEN
    EXECUTE $q$INSERT INTO terminals (id, merchant_user_id, label, key_hash, created_by)
               VALUES ('0192f0c0-0000-7000-8000-00000000a004', '0192f0c0-0000-7000-8000-00000000e001', 't', '\x01',
                       '0192f0c0-0000-7000-8000-00000000e001')$q$;
    EXECUTE $q$UPDATE terminals SET revoked_at = now(), last_used_at = now()
               WHERE id = '0192f0c0-0000-7000-8000-00000000a004'$q$;
  END IF;
END $$;
ROLLBACK;
SQL
); then
  ok "ledger posting, sealing, relay, retention, auth and audit DML"
else
  bad "app DML failed: $(grep -m1 -i error <<<"$out")"
fi

echo "==> default privileges for tables created by later migrations"
as_owner -c "CREATE TABLE zz_future_table (id int PRIMARY KEY, v text)" \
         -c "CREATE SEQUENCE zz_future_seq"
if as_app -c "INSERT INTO zz_future_table VALUES (1, 'a')" \
          -c "UPDATE zz_future_table SET v = 'b'" \
          -c "DELETE FROM zz_future_table" \
          -c "SELECT nextval('zz_future_seq')" >/dev/null 2>&1; then
  ok "payment_app has DML on a table created after 0027"
else
  bad "payment_app lacks DML on a table created after 0027"
fi
expect_denied "DROP a later table" "DROP TABLE zz_future_table"
as_owner -c "DROP TABLE zz_future_table" -c "DROP SEQUENCE zz_future_seq"

echo "==> role attributes"
attrs=$(as_super -d "$db" -tAc "SELECT string_agg(rolname, ',') FROM pg_roles
          WHERE rolname LIKE 'payment\_%' AND (rolsuper OR rolcreaterole OR rolcreatedb OR rolreplication OR rolbypassrls)")
if [[ -z "$attrs" ]]; then ok "no payment_* role is privileged"; else bad "privileged roles: $attrs"; fi
owner=$(as_super -d "$db" -tAc "SELECT string_agg(DISTINCT pg_get_userbyid(relowner), ',') FROM pg_class c
          JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'public' AND relkind IN ('r','S','p')")
if [[ "$owner" == payment_owner ]]; then ok "every table/sequence is owned by payment_owner"; else bad "owners: $owner"; fi
if PGPASSWORD=$(cat "$work/pg_backup_password") psql -X -q -v ON_ERROR_STOP=1 -U payment_backup -d "$db" \
     -c "SELECT count(*) FROM entries" -c "SELECT count(*) FROM _sqlx_migrations" >/dev/null 2>&1; then
  ok "payment_backup can read everything"
else
  bad "payment_backup cannot read"
fi
if PGPASSWORD=$(cat "$work/pg_backup_password") psql -X -q -U payment_backup -d "$db" \
     -c "INSERT INTO transactions (id) VALUES (gen_random_uuid())" >/dev/null 2>&1; then
  bad "payment_backup can WRITE"
else
  ok "payment_backup is read-only"
fi

echo
echo "passed: $pass   failed: $fail"
[[ $fail -eq 0 ]]
