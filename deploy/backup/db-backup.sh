#!/bin/bash
# One encrypted logical dump of the ledger database. Run by backup-loop.sh on
# its schedule, and by deploy.sh before every rollout (--label pre-deploy-<tag>).
#
#   db-backup.sh                      -> /backups/db/daily/payment-<UTC>.sql.gz.age
#   db-backup.sh --label pre-deploy-x -> /backups/db/pre-deploy/payment-<UTC>-pre-deploy-x.sql.gz.age
#
# The dump is plain SQL (pg_dump --no-owner, privileges kept so the
# payment_app grants survive a restore), gzip-compressed, then encrypted
# (backup-lib.sh). It runs as payment_backup, a read-only role with
# pg_read_all_data. Any failing stage — pg_dump, gzip, encryption, a missing
# end-of-dump trailer — fails the run: the partial file is removed, the
# failure is recorded (healthcheck + metrics) and the exit code is non-zero.
#
# Retention: daily KEEP_DAYS, Sunday copies KEEP_WEEKS, 1st-of-month copies
# KEEP_MONTHS (hard links), pre-deploy KEEP_PREDEPLOY_DAYS. Unencrypted dumps
# left by the pre-2026-10 sidecar (/backups/{last,daily,weekly,monthly}) are
# deleted after BACKUP_LEGACY_KEEP_DAYS.
set -euo pipefail
KIND=db
# shellcheck source=backup-lib.sh source-path=SCRIPTDIR
. "$(dirname "$0")/backup-lib.sh"

label=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --label) label=${2:?--label needs a value}; shift 2 ;;
    *) die "unknown argument: $1" ;;
  esac
done
label=$(printf '%s' "$label" | tr -c 'A-Za-z0-9._-' '-')

KEEP_DAYS=${BACKUP_KEEP_DAYS:-14}
KEEP_WEEKS=${BACKUP_KEEP_WEEKS:-8}
KEEP_MONTHS=${BACKUP_KEEP_MONTHS:-12}
KEEP_PREDEPLOY_DAYS=${BACKUP_KEEP_PREDEPLOY_DAYS:-30}
LEGACY_KEEP_DAYS=${BACKUP_LEGACY_KEEP_DAYS:-30}

export PGHOST=${PGHOST:-postgres} PGPORT=${PGPORT:-5432}
export PGUSER=${PGUSER:-payment_backup} PGDATABASE=${PGDATABASE:-payment}
export PGCONNECT_TIMEOUT=${PGCONNECT_TIMEOUT:-15}
PGPASSWORD=$(read_secret_file "${PGPASSWORD_FILE:-/run/secrets/pg_backup_password}")
export PGPASSWORD

require_writable_root
select_encryption
record_attempt

stamp=$(date -u +%Y%m%dT%H%M%SZ)
if [[ -n "$label" ]]; then
  dir="$BACKUP_ROOT/db/pre-deploy"; name="payment-$stamp-$label.sql.gz$EXT"
else
  dir="$BACKUP_ROOT/db/daily";      name="payment-$stamp.sql.gz$EXT"
fi
mkdir -p "$dir"
out="$dir/$name"
part="$dir/.$name.part"
trailer=$(mktemp /tmp/dump-trailer.XXXXXX)
fifo=$(mktemp -u /tmp/dump-fifo.XXXXXX)
cleanup() { rm -f "$part" "$trailer" "$fifo"; [[ -z "${TMP_GNUPGHOME:-}" ]] || rm -rf "$TMP_GNUPGHOME"; }
trap cleanup EXIT
mkfifo -m 600 "$fifo"

log "dumping $PGDATABASE@$PGHOST as $PGUSER -> ${out#"$BACKUP_ROOT"/} (encryption: $MODE)"
started=$(date -u +%s)
# The last bytes of the plain SQL stream are kept aside to check pg_dump's
# end-of-dump trailer (the encrypted file itself cannot be read back here).
tail -c 512 < "$fifo" > "$trailer" &
tail_pid=$!
set +e
pg_dump --no-owner --format=plain --no-password \
  | tee "$fifo" \
  | gzip -6 \
  | encrypt > "$part"
st=("${PIPESTATUS[@]}")
wait "$tail_pid"
set -e
stages=(pg_dump tee gzip encrypt)
failed=""
for i in "${!st[@]}"; do
  [[ "${st[$i]}" -eq 0 ]] || failed+=" ${stages[$i]}=${st[$i]}"
done
[[ -z "$failed" ]] || die "dump pipeline failed (exit codes:$failed); no dump written"
grep -q 'PostgreSQL database dump complete' "$trailer" \
  || die "the dump has no end-of-dump trailer (truncated?); no dump written"
[[ -s "$part" ]] || die "empty dump"

mv "$part" "$out"
(cd "$dir" && sha256sum "$name" > "$name.sha256")
record_success "$out"
log "wrote ${out#"$BACKUP_ROOT"/} ($(du -h "$out" | cut -f1), $(( $(date -u +%s) - started ))s)"

# --- retention (only after a success, so the newest dump always survives) ----
if [[ -z "$label" ]]; then
  if [[ "$(date -u +%u)" == 7 ]]; then
    mkdir -p "$BACKUP_ROOT/db/weekly"
    ln -f "$out" "$BACKUP_ROOT/db/weekly/$name"; ln -f "$out.sha256" "$BACKUP_ROOT/db/weekly/$name.sha256"
  fi
  if [[ "$(date -u +%d)" == 01 ]]; then
    mkdir -p "$BACKUP_ROOT/db/monthly"
    ln -f "$out" "$BACKUP_ROOT/db/monthly/$name"; ln -f "$out.sha256" "$BACKUP_ROOT/db/monthly/$name.sha256"
  fi
fi
prune "$BACKUP_ROOT/db/daily"      "$KEEP_DAYS"              'payment-*'
prune "$BACKUP_ROOT/db/weekly"     "$((KEEP_WEEKS * 7))"     'payment-*'
prune "$BACKUP_ROOT/db/monthly"    "$((KEEP_MONTHS * 31))"   'payment-*'
prune "$BACKUP_ROOT/db/pre-deploy" "$KEEP_PREDEPLOY_DAYS"    'payment-*'
find "$BACKUP_ROOT/db" -name '.*.part' -mmin +720 -delete 2>/dev/null || true
for legacy in last daily weekly monthly; do
  prune "$BACKUP_ROOT/$legacy" "$LEGACY_KEEP_DAYS" '*.sql.gz'
done

offsite_copy "$out" "$out.sha256"
