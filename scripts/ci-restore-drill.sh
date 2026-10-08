#!/usr/bin/env bash
# CI end-to-end test of the backup -> restore-drill pipeline, with the images
# built in this CI run and a database CI fills itself:
#   1. Postgres as in production (same pinned image, run as 999, roles created
#      by deploy/postgres/10-roles.sh from secret files);
#   2. payment-server migrates as payment_owner and serves as payment_app;
#   3. real API traffic: users, an admin deposit, a transfer, a KYC document +
#      submission;
#   4. payment-workers seals checkpoints with a known signing key;
#   5. the backup image dumps as payment_backup (age-encrypted) and archives the
#      KYC volume — exactly the sidecar scripts;
#   6. deploy/restore-drill.sh must PASS: fresh, decrypts, restores, reconciles,
#      `payment-workers verify-chain` intact against the derived public key,
#      every submitted KYC document in the archive;
#   7. negative: a ledger edit that keeps balances consistent (so only the
#      signed checkpoint chain can notice) is dumped again -> the drill must
#      FAIL with a broken chain.
#
#   scripts/ci-restore-drill.sh            (IMAGE_TAG defaults to "ci")
# Needs docker, curl, jq, openssl. Cleans up everything it creates.
set -euo pipefail
cd "$(dirname "$0")/.."
TAG=${IMAGE_TAG:-ci}
APP_IMAGE=payment-system:$TAG
BACKUP_IMAGE=payment-backup:$TAG
PG_IMAGE=$(sed -n 's/^[[:space:]]*image:[[:space:]]*\(postgres:[^[:space:]]*\).*/\1/p' deploy/docker-compose.prod.yml | head -n1)
P=ci-e2e-$$
NET=$P-net
API=http://127.0.0.1:18080
W=$(mktemp -d)

step() { echo; echo "=== $*"; }
cleanup() {
  local rc=$?
  if [[ $rc -ne 0 ]]; then
    for c in "$P-api" "$P-workers" "$P-pg"; do
      echo "--- docker logs $c (tail)"; docker logs --tail 60 "$c" 2>&1 || true
    done
  fi
  docker rm -f "$P-api" "$P-workers" "$P-pg" >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
  docker volume rm "$P-kyc" "$P-backups" >/dev/null 2>&1 || true
  rm -rf "$W"
  exit "$rc"
}
trap cleanup EXIT

hex() { openssl rand -hex "$1"; }
# shellcheck source=deploy/lib.sh
. deploy/lib.sh   # ed25519_public_hex

step "secrets (throwaway)"
mkdir -p "$W/secrets"
for s in postgres_password pg_owner_password pg_app_password pg_backup_password pg_monitor_password; do
  hex 24 > "$W/secrets/$s"
done
SIGNING_KEY=$(hex 32)
ed25519_public_hex "$SIGNING_KEY" > "$W/trusted_public_keys"
docker run --rm --network none --entrypoint age-keygen "$BACKUP_IMAGE" > "$W/age-identity" 2>/dev/null
chmod 600 "$W/age-identity"
sed -n 's/^# public key: //p' "$W/age-identity" > "$W/secrets/backup_recipients"
chmod 644 "$W"/secrets/*
owner_pw=$(cat "$W/secrets/pg_owner_password"); app_pw=$(cat "$W/secrets/pg_app_password")
echo "trusted key: $(cat "$W/trusted_public_keys")  backup recipient: $(cat "$W/secrets/backup_recipients")"

step "postgres ($PG_IMAGE) with deploy/postgres/10-roles.sh"
docker network create "$NET" >/dev/null
docker volume create "$P-kyc" >/dev/null
docker volume create "$P-backups" >/dev/null
mounts=()
for s in postgres_password pg_owner_password pg_app_password pg_backup_password pg_monitor_password; do
  mounts+=(-v "$W/secrets/$s:/run/secrets/$s:ro")
done
docker run -d --name "$P-pg" --network "$NET" --user 999:999 \
  -e POSTGRES_USER=payment -e POSTGRES_PASSWORD_FILE=/run/secrets/postgres_password -e POSTGRES_DB=payment \
  "${mounts[@]}" -v "$PWD/deploy/postgres:/docker-entrypoint-initdb.d:ro" \
  --tmpfs /var/lib/postgresql/data:rw,size=1g,uid=999,gid=999 \
  "$PG_IMAGE" >/dev/null
for _ in $(seq 1 90); do
  docker exec "$P-pg" pg_isready -h 127.0.0.1 -U payment -d payment >/dev/null 2>&1 && break
  sleep 1
done
docker exec "$P-pg" pg_isready -h 127.0.0.1 -U payment -d payment
sql() { docker exec -i "$P-pg" psql -X -qtA -v ON_ERROR_STOP=1 -U payment -d payment "$@"; }
sql -c "SELECT string_agg(rolname, ',' ORDER BY rolname) FROM pg_roles WHERE rolname LIKE 'payment\_%'"

step "payment-server: migrations as payment_owner, requests as payment_app"
docker run -d --name "$P-api" --network "$NET" -p 127.0.0.1:18080:8080 \
  -e APP_ENV=dev -e RUST_LOG=info \
  -e DATABASE_URL="postgres://payment_app:$app_pw@$P-pg:5432/payment" \
  -e MIGRATION_DATABASE_URL="postgres://payment_owner:$owner_pw@$P-pg:5432/payment" \
  -e JWT_SECRET="$(hex 32)" -e DEPOSIT_DUAL_CONTROL=false \
  -e DOCUMENT_STORE_DIR=/data/kyc-docs -v "$P-kyc:/data/kyc-docs" \
  "$APP_IMAGE" >/dev/null
for _ in $(seq 1 120); do curl -fs "$API/ready" >/dev/null 2>&1 && break; sleep 1; done
curl -fsS "$API/ready"; echo
sql -c "SELECT 'migrations applied: ' || count(*) FROM _sqlx_migrations WHERE success"
sql -c "SELECT 'schema owners: ' || string_agg(DISTINCT pg_get_userbyid(relowner), ',') FROM pg_class c
        JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'public' AND relkind IN ('r','S')"

step "API traffic"
uuid() { cat /proc/sys/kernel/random/uuid; }
register() { # register <phone> -> access token
  curl -fsS -X POST "$API/v1/auth/register" -H 'content-type: application/json' \
    -d "{\"phone\":\"$1\",\"password\":\"ci-password-$1\"}" | jq -r .access_token
}
wallet_of() { curl -fsS "$API/v1/wallets" -H "Authorization: Bearer $1" | jq -r '.[0].id'; }
admin=$(register 992900000001); approver=$(register 992900000003)
alice=$(register 992900000002); kycuser=$(register 992900000004)
sql -c "UPDATE users SET is_admin = true, kyc_level = 2 WHERE phone IN ('992900000001', '992900000003')"
sql -c "UPDATE users SET kyc_level = 2 WHERE phone = '992900000002'"
admin_wallet=$(wallet_of "$admin"); alice_wallet=$(wallet_of "$alice")
deposit_id=$(uuid)
dep=$(curl -fsS -X POST "$API/v1/deposits" -H "Authorization: Bearer $admin" -H 'content-type: application/json' \
  -H "Idempotency-Key: $deposit_id" \
  -d "{\"user_account\":\"$alice_wallet\",\"amount_minor\":500000,\"currency\":\"TJS\"}")
echo "deposit: $dep"
if [[ "$(jq -r .status <<<"$dep")" == pending_approval ]]; then
  curl -fsS -X POST "$API/v1/admin/deposits/$(jq -r .id <<<"$dep")/approve" \
    -H "Authorization: Bearer $approver" | jq -c .
fi
curl -fsS -X POST "$API/v1/transfers" -H "Authorization: Bearer $alice" -H 'content-type: application/json' \
  -H "Idempotency-Key: $(uuid)" \
  -d "{\"from_account\":\"$alice_wallet\",\"to_account\":\"$admin_wallet\",\"amount_minor\":12345,\"currency\":\"TJS\"}" | jq -c .
printf '%%PDF-1.4\n%% ci document\n' > "$W/doc.pdf"
doc=$(curl -fsS -X POST "$API/v1/kyc/documents" -H "Authorization: Bearer $kycuser" \
  -F "file=@$W/doc.pdf;type=application/pdf" | jq -r .document_ref)
curl -fsS -X POST "$API/v1/kyc/submissions" -H "Authorization: Bearer $kycuser" -H 'content-type: application/json' \
  -d "{\"requested_level\":1,\"full_name\":\"CI User\",\"document_type\":\"passport\",\"document_ref\":\"$doc\"}" | jq -c .
# Age the document so the drill's "every submitted document is archived" check covers it.
sql -c "UPDATE kyc_documents SET created_at = now() - interval '1 hour' WHERE document_ref = '$doc'"
sql -c "SELECT 'transactions: ' || count(*) FROM transactions"

step "payment-workers: seal with the known key"
docker run -d --name "$P-workers" --network "$NET" \
  -e APP_ENV=dev -e RUST_LOG=info \
  -e DATABASE_URL="postgres://payment_app:$app_pw@$P-pg:5432/payment" \
  -e WORKER_SIGNING_KEY="$SIGNING_KEY" -e WORKER_TRUSTED_PUBLIC_KEYS="$(cat "$W/trusted_public_keys")" \
  -e WORKER_INTERVAL_SECS=1 -e WORKER_HEARTBEAT_FILE=/tmp/heartbeat --read-only --tmpfs /tmp \
  --entrypoint /usr/local/bin/payment-workers "$APP_IMAGE" >/dev/null
sealed=0
for _ in $(seq 1 90); do
  if [[ "$(sql -c "SELECT count(*) = 0 FROM transactions WHERE sealed_seq IS NULL")" == t \
        && "$(sql -c "SELECT count(*) > 0 FROM checkpoints")" == t ]]; then sealed=1; break; fi
  sleep 1
done
[[ $sealed -eq 1 ]] || { echo "workers did not seal every transaction"; exit 1; }
sleep 3
docker stop -t 20 "$P-workers" >/dev/null
if docker logs "$P-workers" 2>&1 | grep -E 'CHAIN VERIFICATION FAILED|RECONCILIATION FAILED'; then
  echo "workers tripped an integrity alarm on an untouched ledger"; exit 1
fi
sql -c "SELECT 'checkpoints: ' || count(*) FROM checkpoints"

backup() { # backup db|kyc
  local common=(--rm -v "$P-backups:/backups" -v "$W/secrets/backup_recipients:/run/secrets/backup_recipients:ro")
  if [[ $1 == db ]]; then
    docker run "${common[@]}" --network "$NET" \
      -v "$W/secrets/pg_backup_password:/run/secrets/pg_backup_password:ro" \
      -e PGHOST="$P-pg" -e PGUSER=payment_backup -e PGDATABASE=payment \
      --entrypoint /usr/local/bin/db-backup.sh "$BACKUP_IMAGE"
  else
    docker run "${common[@]}" --network none -v "$P-kyc:/src:ro" -e KYC_SETTLE_MINUTES=0 \
      --entrypoint /usr/local/bin/kyc-backup.sh "$BACKUP_IMAGE"
  fi
}

step "backups with the sidecar image (payment_backup role, age)"
backup db
backup kyc
docker run --rm -v "$P-backups:/backups:ro" --entrypoint /usr/local/bin/backup-healthcheck.sh "$BACKUP_IMAGE" db
docker run --rm -v "$P-backups:/backups:ro" --entrypoint /usr/local/bin/backup-healthcheck.sh "$BACKUP_IMAGE" kyc

drill() {
  BACKUPS_VOLUME="$P-backups" IMAGE_TAG="$TAG" PG_IMAGE="$PG_IMAGE" \
    deploy/restore-drill.sh -i "$W/age-identity" -k "$W/trusted_public_keys" --max-age 2
}

step "restore drill (must PASS)"
drill

step "tamper: edit a deposit consistently (balances still reconcile), dump again"
sql <<'SQL'
SET session_replication_role = replica;  -- bypass the append-only triggers, as an attacker with superuser would
WITH t AS (
  SELECT e.transaction_id FROM entries e JOIN accounts a ON a.id = e.account_id
  WHERE a.account_type = 'system_settlement' LIMIT 1
), upd AS (
  UPDATE entries SET amount_minor = amount_minor + 100
  WHERE transaction_id = (SELECT transaction_id FROM t)
  RETURNING account_id, direction
)
UPDATE balances b SET raw_minor = b.raw_minor + CASE u.direction WHEN 'credit' THEN 100 ELSE -100 END
FROM upd u WHERE b.account_id = u.account_id;
SQL
sleep 1
backup db
step "restore drill on the tampered dump (must FAIL on the chain only)"
set +e
out=$(drill 2>&1)
rc=$?
set -e
echo "$out"
[[ $rc -eq 1 ]] || { echo "expected the drill to FAIL (exit 1), got $rc"; exit 1; }
grep -q 'ok: conservation' <<<"$out" || { echo "conservation should still hold"; exit 1; }
grep -q 'ok: integrity' <<<"$out" || { echo "balances should still match entries"; exit 1; }
grep -q 'checkpoint chain BROKEN' <<<"$out" || { echo "the chain verification did not catch the edit"; exit 1; }

echo
echo "CI restore drill: PASS (good backup verified, tampered backup rejected by the signed chain)"
