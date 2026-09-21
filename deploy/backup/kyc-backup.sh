#!/bin/sh
# Nightly tarball of the KYC document volume into the shared `backups` volume.
# Runs from cron inside the `kyc-backup` sidecar (alpine); POSIX sh only.
#
#   /src            the kycdocs volume, mounted read-only
#   /backups/kyc-docs/kyc-docs-<UTC stamp>.tar.gz   (+ kyc-docs-latest.tar.gz link)
#
# Run by hand:  docker compose exec kyc-backup /usr/local/bin/kyc-backup.sh
set -eu

SRC=${KYC_SRC:-/src}
DEST=${KYC_BACKUP_DIR:-/backups/kyc-docs}
KEEP_DAYS=${KYC_BACKUP_KEEP_DAYS:-14}

mkdir -p "$DEST"
STAMP=$(date -u +%Y%m%d-%H%M%S)
OUT="$DEST/kyc-docs-$STAMP.tar.gz"
TMP="$OUT.part"

# Write to a temp name and rename, so a reader never sees a half-written file.
# tar exits 1 when a file changes while being read (an upload in flight): the
# archive is still complete for every other file, so that is not fatal.
rc=0
tar -C "$SRC" -czf "$TMP" . || rc=$?
if [ "$rc" -eq 1 ]; then
    echo "kyc-backup: warning: a file changed while it was being archived" >&2
elif [ "$rc" -ne 0 ]; then
    rm -f "$TMP"
    echo "kyc-backup: tar failed (exit $rc)" >&2
    exit "$rc"
fi
mv "$TMP" "$OUT"
ln -sfn "$(basename "$OUT")" "$DEST/kyc-docs-latest.tar.gz"

# Retention: drop archives older than KEEP_DAYS (the `latest` link is re-pointed
# on every run, so it never dangles for long).
find "$DEST" -maxdepth 1 -type f -name 'kyc-docs-*.tar.gz' -mtime +"$KEEP_DAYS" -exec rm -f {} \;

echo "kyc-backup: wrote $OUT ($(du -h "$OUT" | cut -f1)); retention ${KEEP_DAYS}d"
