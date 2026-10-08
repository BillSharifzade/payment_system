#!/bin/bash
# One encrypted tarball of the KYC document volume (identity documents are
# files, not rows: pg_dump alone would lose them). Run by backup-loop.sh.
#
#   /src                     the kycdocs volume, mounted read-only
#   /backups/kyc-docs/kyc-docs-<UTC>.tar.gz.age
#
# Fails LOUDLY: any non-zero exit from find, tar, gzip or the encryption is a
# failed backup — the partial archive is deleted, the failure is recorded
# (healthcheck + metrics) and the exit code is non-zero. Documents younger than
# KYC_SETTLE_MINUTES are left for the next run, so an upload still being
# written is never archived half-way; a file removed between listing and
# archiving (orphan pruning) makes the run retry once with a fresh listing.
#
# Run by hand:  docker compose exec kyc-backup /usr/local/bin/kyc-backup.sh
set -euo pipefail
KIND=kyc
# shellcheck source=backup-lib.sh source-path=SCRIPTDIR
. "$(dirname "$0")/backup-lib.sh"

SRC=${KYC_SRC:-/src}
DEST="$BACKUP_ROOT/kyc-docs"
KEEP_DAYS=${KYC_BACKUP_KEEP_DAYS:-14}
SETTLE_MINUTES=${KYC_SETTLE_MINUTES:-2}

[[ -d "$SRC" ]] || die "$SRC is not a directory (is the kycdocs volume mounted?)"
require_writable_root
select_encryption
record_attempt
mkdir -p "$DEST"

stamp=$(date -u +%Y%m%dT%H%M%SZ)
name="kyc-docs-$stamp.tar.gz$EXT"
out="$DEST/$name"
part="$DEST/.$name.part"
list=$(mktemp /tmp/kyc-list.XXXXXX)
cleanup() { rm -f "$part" "$list"; [[ -z "${TMP_GNUPGHOME:-}" ]] || rm -rf "$TMP_GNUPGHOME"; }
trap cleanup EXIT

attempt() {
  rm -f "$part"
  (cd "$SRC" && find . -type f -mmin +"$SETTLE_MINUTES" -print0) > "$list" || return 10
  items=$(tr -cd '\0' < "$list" | wc -c)
  set +e
  tar --create --file=- --null --no-recursion --directory="$SRC" --files-from="$list" \
    | gzip -6 \
    | encrypt > "$part"
  local st=("${PIPESTATUS[@]}")
  set -e
  local stages=(tar gzip encrypt) i failed=""
  for i in "${!st[@]}"; do
    [[ "${st[$i]}" -eq 0 ]] || failed+=" ${stages[$i]}=${st[$i]}"
  done
  if [[ -n "$failed" ]]; then
    log "archive pipeline failed (exit codes:$failed)"
    return 1
  fi
  [[ -s "$part" ]] || { log "empty archive"; return 1; }
}

items=0
if ! attempt; then
  log "retrying once in ${KYC_RETRY_DELAY_SECS:-30} s with a fresh file list"
  sleep "${KYC_RETRY_DELAY_SECS:-30}"
  attempt || die "KYC archive failed twice; no archive written (see the errors above)"
fi

mv "$part" "$out"
(cd "$DEST" && sha256sum "$name" > "$name.sha256")
record_success "$out" "$items"
log "wrote ${out#"$BACKUP_ROOT"/} ($items files, $(du -h "$out" | cut -f1)); retention ${KEEP_DAYS}d"

# Retention (also ages out the unencrypted kyc-docs-*.tar.gz of the old sidecar).
prune "$DEST" "$KEEP_DAYS" 'kyc-docs-*'
find "$DEST" -name '.*.part' -mmin +720 -delete 2>/dev/null || true

offsite_copy "$out" "$out.sha256"
