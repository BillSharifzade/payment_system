#!/usr/bin/env bash
# Restore drill: prove the newest backup is FRESH, DECRYPTABLE, RESTORABLE, that
# the ledger inside it is consistent, that its signed checkpoint chain verifies
# against the trusted public keys, and that the newest KYC archive is intact
# and holds every submitted document. Run it monthly (host cron), after any
# change to the backups, and on a machine OFF the box with copies fetched back
# from off-box storage. Nothing in the live stack is touched: the dump is
# restored into a throwaway Postgres on tmpfs.
#
#   ./restore-drill.sh                       newest dump + newest KYC archive in the backups volume
#   ./restore-drill.sh DUMP [--kyc ARCHIVE]  specific files (e.g. fetched back from off-box)
#
# Options
#   -i, --identity FILE      age identity for *.age files (default: secrets/backup_age_identity).
#                            gpg files are decrypted with your own keyring (GNUPGHOME).
#   -k, --trusted-keys FILE  trusted Ed25519 public keys, comma/newline-separated hex
#                            (default: secrets/worker_trusted_public_keys — off the box,
#                            use your off-box copy, not one read from the server)
#   --max-age HOURS          refuse a dump or archive older than this (default:
#                            RESTORE_MAX_AGE_HOURS or 30; 0 = no limit)
#   --kyc FILE|none          KYC archive to test (default: newest in the backups)
#   --skip-chain             do not run the chain verification (exit 3 = partial pass)
#   --backups-dir DIR        read backups from a host directory (same layout as the
#                            volume: db/daily, db/pre-deploy, kyc-docs) instead of the volume
#   --pg-url URL             restore into this SCRATCH database instead of a throwaway
#                            container — it is DROPPED and recreated (superuser URL)
#   --workers-bin PATH       run verify-chain with this local payment-workers binary
#                            instead of the image
# Environment: PG_IMAGE, WORKERS_IMAGE, BACKUP_IMAGE, BACKUPS_VOLUME, IMAGE_TAG,
#   MIN_MIGRATIONS, DRILL_TMPFS_SIZE, GNUPGHOME
#
# Exit: 0 PASS, 1 FAIL, 2 usage/environment error, 3 PASS with checks skipped.
#
# Checks
#   0. freshness     — the dump (and archive) is younger than --max-age; the age
#                      comes from the UTC stamp in the file name, else its mtime
#   1. format        — gzip / plain SQL / pg_dump custom, optionally inside age or
#                      gpg (detected from the bytes, not the file name)
#   2. restore       — psql/pg_restore with ON_ERROR_STOP into an empty database
#   3. shape         — migration table, ledger tables, migration count
#   4. conservation  — per currency, SUM(balances.raw_minor) = 0
#   5. integrity     — every balance equals the signed sum of its entries
#                      (a database served by LEDGER_BACKEND=tigerbeetle keeps its
#                      balances in the cluster, not in the dump: there 4 sums the
#                      entries per currency and 5 checks every transaction balances)
#   6. chain         — `payment-workers verify-chain` (the workers image) against
#                      the restored database and the trusted public keys
#   7. KYC archive   — decrypts and lists completely (gzip CRC checked); every
#                      document referenced by a KYC submission created before the
#                      archive is in it
set -euo pipefail
cd "$(dirname "$0")"
# shellcheck source=lib.sh source-path=SCRIPTDIR
. ./lib.sh

DUMP_ARG=""
IDENTITY=""
TRUSTED=""
MAX_AGE_H=${RESTORE_MAX_AGE_HOURS:-30}
KYC_ARG=""
SKIP_CHAIN=0
BACKUPS_DIR=""
PG_URL=""
WORKERS_BIN=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    -i|--identity)     IDENTITY=${2:?}; shift 2 ;;
    -k|--trusted-keys) TRUSTED=${2:?}; shift 2 ;;
    --max-age)         MAX_AGE_H=${2:?}; shift 2 ;;
    --kyc)             KYC_ARG=${2:?}; shift 2 ;;
    --skip-chain)      SKIP_CHAIN=1; shift ;;
    --backups-dir)     BACKUPS_DIR=${2:?}; shift 2 ;;
    --pg-url)          PG_URL=${2:?}; shift 2 ;;
    --workers-bin)     WORKERS_BIN=${2:?}; shift 2 ;;
    -h|--help)         sed -n '2,46p' "$0"; exit 0 ;;
    -*)                echo "unknown option $1 (see --help)" >&2; exit 2 ;;
    *)                 DUMP_ARG=$1; shift ;;
  esac
done
[[ "$MAX_AGE_H" =~ ^[0-9]+$ ]] || { echo "--max-age must be whole hours" >&2; exit 2; }
[[ -z "$IDENTITY" && -f secrets/backup_age_identity ]] && IDENTITY=secrets/backup_age_identity
[[ -z "$TRUSTED" && -f secrets/worker_trusted_public_keys ]] && TRUSTED=secrets/worker_trusted_public_keys
# TSA roots for the RFC 3161 checkpoint anchors (DESIGN.md §6.5): with them the
# anchor tokens are verified cryptographically, without them only for imprint.
TSA_CERTS=${ANCHOR_RFC3161_CERTS_FILE:-anchor/tsa-certs.pem}
[[ -s "$TSA_CERTS" ]] || TSA_CERTS=""

PG_IMAGE=${PG_IMAGE:-$(pg_image_ref)}
WORKERS_IMAGE=${WORKERS_IMAGE:-payment-system:${IMAGE_TAG:-latest}}
BACKUP_IMAGE=${BACKUP_IMAGE:-payment-backup:${IMAGE_TAG:-latest}}
BACKUPS_VOLUME=${BACKUPS_VOLUME:-${PROJECT}_backups}
MIN_MIGRATIONS=${MIN_MIGRATIONS:-22}

fail=0
partial=0
FAIL() { echo "FAIL: $*"; fail=1; }
fatal() { echo "FAIL: $*"; echo "FAIL: restore drill aborted"; exit 1; }
ok() { echo "    ok: $*"; }

# ---- scratch database -----------------------------------------------------------
NAME="payment-restore-drill-$$"
NET="$NAME-net"
WORK=$(mktemp -d)
chmod 700 "$WORK"
# shellcheck disable=SC2329  # invoked by the EXIT trap below
cleanup() {
  if [[ -z "$PG_URL" ]]; then
    docker rm -f "$NAME" >/dev/null 2>&1 || true
    docker network rm "$NET" >/dev/null 2>&1 || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

if [[ -n "$PG_URL" && -z "$BACKUPS_DIR" && ( -z "$DUMP_ARG" || -z "$KYC_ARG" ) ]]; then
  echo "--pg-url has no backups volume: pass DUMP and --kyc FILE|none, or --backups-dir DIR" >&2; exit 2
fi
if [[ -n "$PG_URL" ]]; then
  db=${PG_URL##*/}; db=${db%%\?*}
  [[ "$db" =~ ^[a-z_][a-z0-9_]*$ ]] || { echo "--pg-url must name a database" >&2; exit 2; }
  case "$db" in payment|postgres|template0|template1)
    echo "--pg-url names '$db': refusing to drop it — point it at a scratch database" >&2; exit 2 ;;
  esac
  ADMIN_URL="${PG_URL%/*}/postgres"
  echo "==> scratch database $db (dropped and recreated)"
  psql -X -q -v ON_ERROR_STOP=1 "$ADMIN_URL" -c "DROP DATABASE IF EXISTS $db WITH (FORCE)" -c "CREATE DATABASE $db" \
    || fatal "cannot recreate $db"
  q()      { psql -X -tA -v ON_ERROR_STOP=1 "$PG_URL" -c "$1"; }
  # The decrypted dump is staged in a private temp dir on THIS machine (use
  # --pg-url only on a trusted host; the container mode keeps it on tmpfs).
  DUMP_LANDING="$WORK/dump"
  land()   { cat > "$DUMP_LANDING"; }
  landed_head() { head -c 16 "$DUMP_LANDING" | od -An -tx1 -v | tr -d ' \n'; }
  restore_gzip()   { gzip -dc "$DUMP_LANDING" | psql -X -q -v ON_ERROR_STOP=1 "$PG_URL" >/dev/null; }
  restore_plain()  { psql -X -q -v ON_ERROR_STOP=1 "$PG_URL" -f "$DUMP_LANDING" >/dev/null; }
  restore_custom() { pg_restore --exit-on-error --no-owner -d "$PG_URL" "$DUMP_LANDING"; }
  tar_list() { tar -tzf -; }
  VERIFY_URL=$PG_URL
else
  command -v docker >/dev/null || { echo "docker is required (or use --pg-url)" >&2; exit 2; }
  pw=$(openssl rand -hex 16)
  # No egress for the scratch database — unless a local verifier binary must
  # reach it through a loopback port (internal networks cannot publish).
  publish=()
  netopt=(--internal)
  if [[ -n "$WORKERS_BIN" ]]; then publish=(-p 127.0.0.1::5432); netopt=(); fi
  docker network create "${netopt[@]}" "$NET" >/dev/null
  vol=()
  [[ -z "$BACKUPS_DIR" ]] && vol=(-v "$BACKUPS_VOLUME:/backups:ro")
  echo "==> starting scratch Postgres ($PG_IMAGE) on tmpfs..."
  docker run -d --name "$NAME" --network "$NET" "${publish[@]}" "${vol[@]}" \
    --tmpfs "/var/lib/postgresql/data:rw,size=${DRILL_TMPFS_SIZE:-2g}" \
    --tmpfs "/drill:rw,size=${DRILL_TMPFS_SIZE:-2g},mode=1777" \
    -e POSTGRES_USER=postgres -e POSTGRES_PASSWORD="$pw" -e POSTGRES_DB=payment \
    "$PG_IMAGE" >/dev/null
  # The entrypoint runs a socket-only server for initialisation, stops it and
  # starts the real one; only the real one listens on TCP, so probe TCP.
  for _ in $(seq 1 90); do
    docker exec "$NAME" pg_isready -h 127.0.0.1 -U postgres -d payment >/dev/null 2>&1 && break
    sleep 1
  done
  docker exec "$NAME" pg_isready -h 127.0.0.1 -U postgres -d payment >/dev/null \
    || { docker logs --tail 30 "$NAME" >&2; fatal "scratch postgres did not start"; }
  q()      { docker exec "$NAME" psql -X -tA -v ON_ERROR_STOP=1 -U postgres -d payment -c "$1"; }
  # The decrypted dump is staged on the scratch container's tmpfs (memory).
  DUMP_LANDING=/drill/dump
  land()   { docker exec -i "$NAME" sh -c 'cat > /drill/dump'; }
  landed_head() { docker exec "$NAME" sh -c 'head -c 16 /drill/dump | od -An -tx1 -v' | tr -d ' \n'; }
  restore_gzip()   { docker exec "$NAME" bash -c 'set -o pipefail; gzip -dc /drill/dump | psql -X -q -v ON_ERROR_STOP=1 -U postgres -d payment >/dev/null'; }
  restore_plain()  { docker exec "$NAME" psql -X -q -v ON_ERROR_STOP=1 -U postgres -d payment -f /drill/dump >/dev/null; }
  restore_custom() { docker exec "$NAME" pg_restore --exit-on-error --no-owner -U postgres -d payment /drill/dump; }
  tar_list() { docker exec -i "$NAME" tar -tzf -; }
  if [[ -n "$WORKERS_BIN" ]]; then
    port=$(docker port "$NAME" 5432/tcp | head -n1 | sed 's/.*://')
    VERIFY_URL="postgres://postgres:$pw@127.0.0.1:$port/payment"
  else
    VERIFY_URL="postgres://postgres:$pw@$NAME:5432/payment"
  fi
fi

# ---- reading backups (volume, host directory or explicit file) -------------------
# list_backups <glob...>: "<mtime epoch> <path>" for files under the backups root
list_backups() {
  local args=()
  local g
  for g in "$@"; do args+=(-o -name "$g"); done
  if [[ -n "$BACKUPS_DIR" ]]; then
    find "$BACKUPS_DIR/db/daily" "$BACKUPS_DIR/db/pre-deploy" "$BACKUPS_DIR/kyc-docs" \
         "$BACKUPS_DIR/last" "$BACKUPS_DIR/daily" -maxdepth 1 -type f \
         \( -false "${args[@]}" \) ! -name '*.sha256' ! -name '.*' -printf '%T@ %p\n' 2>/dev/null || true
  else
    docker exec "$NAME" sh -c 'find /backups/db/daily /backups/db/pre-deploy /backups/kyc-docs /backups/last /backups/daily \
         -maxdepth 1 -type f "$@" ! -name "*.sha256" ! -name ".*" -printf "%T@ %p\n" 2>/dev/null || true' \
      sh \( -false "${args[@]}" \)
  fi
}
# src_cat <path>: stream a file (a path from list_backups, or a host file)
src_cat() {
  if [[ -f "$1" ]]; then cat "$1"
  elif [[ -z "$BACKUPS_DIR" && -z "$PG_URL" ]]; then docker exec "$NAME" cat "$1"
  else cat "$1"; fi
}
newest() { sort -rn | head -n1 | cut -d' ' -f2-; }
mtime_of_listed() { # mtime_of_listed <listing> <path>
  printf '%s\n' "$1" | awk -v p="$2" '{ m=$1; $1=""; sub(/^ /, ""); if ($0 == p) { printf "%d", m; exit } }'
}

# stamp_epoch <file name>: epoch from a UTC stamp in the name, or empty
stamp_epoch() {
  local b; b=$(basename "$1")
  if [[ "$b" =~ ([0-9]{8})T([0-9]{6})Z ]]; then
    date -u -d "${BASH_REMATCH[1]} ${BASH_REMATCH[2]:0:2}:${BASH_REMATCH[2]:2:2}:${BASH_REMATCH[2]:4:2}" +%s
  elif [[ "$b" =~ ([0-9]{8})-([0-9]{6}) ]]; then
    date -u -d "${BASH_REMATCH[1]} ${BASH_REMATCH[2]:0:2}:${BASH_REMATCH[2]:2:2}:${BASH_REMATCH[2]:4:2}" +%s
  fi
}

# check_age <what> <path> <mtime fallback>: sets AGE_EPOCH; FAILs when stale
check_age() {
  local e; e=$(stamp_epoch "$2" || true)
  [[ -n "$e" ]] || e=$3
  AGE_EPOCH=$e
  local age_h=$(( ( $(date -u +%s) - e ) / 3600 ))
  if [[ "$MAX_AGE_H" -gt 0 && "$age_h" -ge "$MAX_AGE_H" ]]; then
    FAIL "$1 $(basename "$2") is ${age_h} h old (limit ${MAX_AGE_H} h) — backups are not running? (override: --max-age)"
    return 1
  fi
  ok "$1 is ${age_h} h old (limit ${MAX_AGE_H} h)"
}

# kind_of <hex of the first bytes>
kind_of() {
  local h=$1
  case "$h" in
    1f8b*) echo gzip ;;
    5047444d50*) echo custom ;;                                   # "PGDMP"
    6167652d656e6372797074696f6e2e6f72672f7631*) echo age ;;      # "age-encryption.org/v1"
    2d2d2d2d2d424547494e20414745*) echo age ;;                    # "-----BEGIN AGE"
    2d2d2d2d2d424547494e20504750*) echo gpg ;;                    # "-----BEGIN PGP"
    8[4-7]*|8[c-f]*|c1*|c3*) echo gpg ;;                          # OpenPGP PKESK/SKESK packet
    *) echo plain ;;
  esac
}

# decrypt <kind>: stdin -> stdout
decrypt() {
  case "$1" in
    age)
      [[ -n "$IDENTITY" && -r "$IDENTITY" ]] \
        || { echo "FAIL: an age identity is needed (-i FILE; default secrets/backup_age_identity)" >&2; return 1; }
      if command -v age >/dev/null 2>&1; then
        age --decrypt --identity "$IDENTITY"
      else
        docker run --rm -i --network none --user "$(id -u):$(id -g)" \
          -v "$(cd "$(dirname "$IDENTITY")" && pwd)/$(basename "$IDENTITY"):/run/identity:ro" \
          --entrypoint age "$BACKUP_IMAGE" --decrypt --identity /run/identity
      fi ;;
    gpg)  gpg --batch --quiet --decrypt ;;
    *)    cat ;;
  esac
}

head_hex() { # head_hex <path>: first 32 bytes as hex
  { src_cat "$1" || true; } | head -c 32 | od -An -tx1 -v | tr -d ' \n'
}

# ---- 0+1. locate, check freshness, decrypt ------------------------------------------
if [[ -n "$DUMP_ARG" ]]; then
  [[ -f "$DUMP_ARG" ]] || fatal "no such file $DUMP_ARG"
  dump=$DUMP_ARG
  dump_mtime=$(stat -c %Y "$dump")
else
  listing=$(list_backups 'payment-*.sql*' '*.sql.gz')
  dump=$(printf '%s\n' "$listing" | grep -v '/kyc-docs/' | grep -v -- '-latest\.' | newest || true)
  [[ -n "$dump" ]] || fatal "no dump found in ${BACKUPS_DIR:-the $BACKUPS_VOLUME volume} (has postgres-backup run yet?)"
  dump_mtime=$(mtime_of_listed "$listing" "$dump")
fi
echo "==> dump: $dump"
check_age "dump" "$dump" "$dump_mtime" || true

outer=$(kind_of "$(head_hex "$dump")")
echo "    format: $outer$( [[ $outer == age || $outer == gpg ]] && echo ' (encrypted)')"
set +e
src_cat "$dump" | decrypt "$outer" | land
st=("${PIPESTATUS[@]}")
set -e
[[ "${st[0]}" -eq 0 || "${st[0]}" -eq 141 ]] || fatal "cannot read $dump"
[[ "${st[1]}" -eq 0 ]] || fatal "decryption failed ($outer) — wrong identity/key, or a corrupt file"
[[ "${st[2]}" -eq 0 ]] || fatal "cannot stage the decrypted dump"
inner=$(kind_of "$(landed_head)")
case "$inner" in
  gzip|plain|custom) ok "decrypted content is $inner" ;;
  *) fatal "decrypted content is $inner — not a dump (encrypted twice?)" ;;
esac

# ---- 2. restore ---------------------------------------------------------------------
# The roles the dump GRANTs to must exist (no login, no password).
q "DO \$\$ BEGIN
     IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'payment_owner')   THEN CREATE ROLE payment_owner NOLOGIN; END IF;
     IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'payment_app')     THEN CREATE ROLE payment_app NOLOGIN; END IF;
     IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'payment_backup')  THEN CREATE ROLE payment_backup NOLOGIN; END IF;
     IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'payment_monitor') THEN CREATE ROLE payment_monitor NOLOGIN; END IF;
   END \$\$" >/dev/null
echo "==> restoring..."
set +e
case "$inner" in
  gzip)   restore_gzip;   rc=$? ;;
  plain)  restore_plain;  rc=$? ;;
  custom) restore_custom; rc=$? ;;
esac
set -e
[[ $rc -eq 0 ]] || fatal "restore reported errors (see above)"
ok "restored with ON_ERROR_STOP"

# ---- 3. shape -------------------------------------------------------------------------
if [[ "$(q "SELECT to_regclass('public.transactions') IS NOT NULL")" != "t" ]]; then
  fatal "the dump contains no ledger tables — was it taken before the server applied migrations?"
fi
migrations=$(q "SELECT COUNT(*) FROM _sqlx_migrations WHERE success")
txns=$(q "SELECT COUNT(*) FROM transactions")
entries=$(q "SELECT COUNT(*) FROM entries")
accounts=$(q "SELECT COUNT(*) FROM balances")
checkpoints=$(q "SELECT COUNT(*) FROM checkpoints")
echo "    migrations=$migrations transactions=$txns entries=$entries accounts=$accounts checkpoints=$checkpoints"
[[ "$migrations" -ge "$MIN_MIGRATIONS" ]] || FAIL "only $migrations migrations applied in the dump (expected >= $MIN_MIGRATIONS)"

# Served by TigerBeetle (DESIGN.md §15): the hybrid has posted, and no rollback rebuilt
# `balances` since. Then `balances` holds its cut-over values by design.
backend=postgres
if [[ "$(q "SELECT to_regclass('public.tb_intents') IS NOT NULL AND EXISTS (SELECT 1 FROM tb_intents)")" == t ]] \
   && [[ "$(q "SELECT NOT EXISTS (SELECT 1 FROM tb_cutover
                                  WHERE rolled_back_at >= (SELECT max(created_at) FROM tb_intents))")" == t ]]; then
  backend=tigerbeetle
  echo "    ledger backend: tigerbeetle (balances live in the cluster; the dump holds the journal)"
fi
signed="CASE direction WHEN 'credit' THEN amount_minor ELSE -amount_minor END"

# ---- 4. conservation --------------------------------------------------------------------
if [[ $backend == postgres ]]; then
  imbalance=$(q "SELECT string_agg(currency || '=' || total, ', ')
                 FROM (SELECT a.currency, SUM(b.raw_minor) AS total
                       FROM balances b JOIN accounts a ON a.id = b.account_id
                       GROUP BY a.currency HAVING SUM(b.raw_minor) <> 0) x")
else
  imbalance=$(q "SELECT string_agg(currency || '=' || total, ', ')
                 FROM (SELECT currency, SUM($signed) AS total FROM entries
                       GROUP BY currency HAVING SUM($signed) <> 0) x")
fi
if [[ -n "$imbalance" ]]; then FAIL "conservation broken: $imbalance"; else ok "conservation (every currency sums to 0)"; fi

# ---- 5. integrity -------------------------------------------------------------------------
if [[ $backend == postgres ]]; then
  mismatches=$(q "SELECT COUNT(*) FROM balances b
                  LEFT JOIN (SELECT account_id, SUM($signed) AS derived
                             FROM entries GROUP BY account_id) d ON d.account_id = b.account_id
                  WHERE b.raw_minor <> COALESCE(d.derived, 0)")
  if [[ "$mismatches" != "0" ]]; then FAIL "$mismatches account balance(s) disagree with their entries"; else ok "integrity (all $accounts balances match their entries)"; fi
else
  unbalanced=$(q "SELECT COUNT(DISTINCT transaction_id) FROM (
                    SELECT transaction_id FROM entries GROUP BY transaction_id, currency
                    HAVING SUM($signed) <> 0) x")
  if [[ "$unbalanced" != "0" ]]; then FAIL "$unbalanced transaction(s) do not balance"; else ok "integrity (every transaction balances; reconcile the restored cluster against this journal)"; fi
fi

# ---- 6. checkpoint chain ----------------------------------------------------------------------
if [[ $SKIP_CHAIN -eq 1 ]]; then
  echo "    SKIPPED: chain verification (--skip-chain)"
  partial=1
elif [[ -z "$TRUSTED" || ! -s "$TRUSTED" ]]; then
  FAIL "no trusted public keys (-k FILE; default secrets/worker_trusted_public_keys) — cannot verify the chain"
else
  keys=$(tr -s ' \r\n\t,' ',' < "$TRUSTED" | sed 's/^,//; s/,$//')
  echo "==> verify-chain against $(tr ',' '\n' <<<"$keys" | grep -c .) trusted key(s)..."
  set +e
  if [[ -n "$WORKERS_BIN" ]]; then
    report=$(APP_ENV=prod DATABASE_URL="$VERIFY_URL" WORKER_TRUSTED_PUBLIC_KEYS="$keys" \
             ANCHOR_RFC3161_CERTS_FILE="$TSA_CERTS" RUST_LOG=warn "$WORKERS_BIN" verify-chain)
    vrc=$?
  else
    tsa=()
    [[ -n "$TSA_CERTS" ]] && tsa=(-v "$(cd "$(dirname "$TSA_CERTS")" && pwd)/$(basename "$TSA_CERTS"):/run/tsa-certs.pem:ro"
                                 -e ANCHOR_RFC3161_CERTS_FILE=/run/tsa-certs.pem)
    report=$(docker run --rm --network "$NET" --read-only --tmpfs /tmp --cap-drop ALL --security-opt no-new-privileges \
             -e APP_ENV=prod -e RUST_LOG=warn -e DATABASE_URL="$VERIFY_URL" -e WORKER_TRUSTED_PUBLIC_KEYS="$keys" \
             "${tsa[@]}" --entrypoint /usr/local/bin/payment-workers "$WORKERS_IMAGE" verify-chain)
    vrc=$?
  fi
  set -e
  printf '%s\n' "$report" | sed 's/^/    /'
  case $vrc in
    0) ok "checkpoint chain intact (signatures by trusted keys, Merkle roots match the restored rows)" ;;
    1) FAIL "checkpoint chain BROKEN in the restored database — possible tampering, or an untrusted signing key" ;;
    *) FAIL "verify-chain could not run (exit $vrc; image $WORKERS_IMAGE)" ;;
  esac
fi

# ---- 7. KYC archive ------------------------------------------------------------------------
if [[ "$KYC_ARG" == none ]]; then
  echo "    SKIPPED: KYC archive (--kyc none)"
  partial=1
else
  if [[ -n "$KYC_ARG" ]]; then
    [[ -f "$KYC_ARG" ]] || fatal "no such file $KYC_ARG"
    kyc=$KYC_ARG; kyc_mtime=$(stat -c %Y "$kyc")
  else
    klisting=$(list_backups 'kyc-docs-*.tar.gz*')
    kyc=$(printf '%s\n' "$klisting" | grep -v -- '-latest\.' | newest || true)
    kyc_mtime=$( [[ -n "$kyc" ]] && mtime_of_listed "$klisting" "$kyc" || true)
  fi
  if [[ -z "$kyc" ]]; then
    FAIL "no KYC archive found (has kyc-backup run yet? --kyc none to skip)"
  else
    echo "==> KYC archive: $kyc"
    if check_age "KYC archive" "$kyc" "$kyc_mtime"; then :; fi
    kyc_epoch=$AGE_EPOCH
    kouter=$(kind_of "$(head_hex "$kyc")")
    set +e
    src_cat "$kyc" | decrypt "$kouter" | tar_list > "$WORK/kyc.list"
    kst=("${PIPESTATUS[@]}")
    set -e
    if [[ "${kst[1]}" -ne 0 ]]; then
      FAIL "KYC archive: decryption failed ($kouter)"
    elif [[ "${kst[2]}" -ne 0 ]]; then
      FAIL "KYC archive: corrupt or truncated (tar/gzip exit ${kst[2]})"
    else
      members=$(grep -cv '/$' "$WORK/kyc.list" || true)
      ok "KYC archive decrypts ($kouter) and lists completely: $members file(s)"
      sed 's#^\./##' "$WORK/kyc.list" | sort -u > "$WORK/kyc.names"
      q "SELECT d.document_ref FROM kyc_documents d
         WHERE d.created_at < to_timestamp($kyc_epoch) - interval '5 minutes'
           AND EXISTS (SELECT 1 FROM kyc_submissions s WHERE s.document_ref = d.document_ref)
         ORDER BY 1" | sort -u > "$WORK/kyc.expected"
      missing=$(comm -23 "$WORK/kyc.expected" "$WORK/kyc.names")
      expected=$(grep -c . "$WORK/kyc.expected" || true)
      if [[ -n "$missing" ]]; then
        FAIL "$(grep -c . <<<"$missing") of $expected submitted KYC document(s) are missing from the archive, e.g. $(head -n3 <<<"$missing" | tr '\n' ' ')"
      else
        ok "all $expected document(s) cited by KYC submissions before the archive are in it"
      fi
    fi
  fi
fi

# ---- verdict ----------------------------------------------------------------------------------
if [[ $fail -ne 0 ]]; then
  echo "FAIL: see above ($dump)"
  exit 1
fi
if [[ $partial -ne 0 ]]; then
  echo "PASS (PARTIAL — some checks were skipped): $dump"
  exit 3
fi
echo "PASS: $dump is fresh, decrypts, restores cleanly, the ledger is consistent and its checkpoint chain verifies"
exit 0
