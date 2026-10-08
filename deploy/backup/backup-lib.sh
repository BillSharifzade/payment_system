# shellcheck shell=bash
# Shared helpers for db-backup.sh and kyc-backup.sh (sourced, bash).
#
# Every run records its outcome in two places:
#   $BACKUP_ROOT/.state/<kind>.*     plain files; backup-healthcheck.sh and the
#                                    scheduler read them (last success decides
#                                    the container's health, not "is cron up")
#   $BACKUP_METRICS_DIR/payment_backup_<kind>.prom
#                                    node-exporter textfile collector format, so
#                                    Prometheus alerts on a stale last success
#
# Encryption (required unless BACKUP_ENCRYPTION=none): the recipients file is
# a compose secret holding either age recipients (one `age1...`/`ssh-ed25519 ...`
# per line) or an ASCII-armoured OpenPGP public key; BACKUP_ENCRYPTION=auto
# (the default) tells them apart. The matching PRIVATE key never lives in this
# container.

: "${KIND:?KIND must be set before sourcing backup-lib.sh}"
BACKUP_ROOT=${BACKUP_ROOT:-/backups}
STATE_DIR="$BACKUP_ROOT/.state"
METRICS_DIR=${BACKUP_METRICS_DIR:-/metrics}
RECIPIENTS_FILE=${BACKUP_RECIPIENTS_FILE:-/run/secrets/backup_recipients}
ENCRYPTION=${BACKUP_ENCRYPTION:-auto}
OFFSITE_REMOTE=${BACKUP_OFFSITE_REMOTE:-}
RCLONE_CONFIG_FILE=${BACKUP_RCLONE_CONFIG:-/run/secrets/backup_rclone_conf}
TMP_GNUPGHOME=""   # set by select_encryption in gpg mode, never inherited

log() { printf '%s %s-backup: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$KIND" "$*" >&2; }

# --- state + metrics ---------------------------------------------------------
state_get() { cat "$STATE_DIR/$KIND.$1" 2>/dev/null || true; }
state_set() { printf '%s\n' "$2" > "$STATE_DIR/$KIND.$1.tmp" && mv "$STATE_DIR/$KIND.$1.tmp" "$STATE_DIR/$KIND.$1"; }

# metric <name> <help> <value>: one gauge (HELP/TYPE always, the sample only
# when the value is known). HELP text must not vary between the db and kyc
# files: the textfile collector rejects inconsistent help for one metric.
metric() {
  echo "# HELP $1 $2"
  echo "# TYPE $1 gauge"
  if [[ -n "$3" ]]; then echo "$1{kind=\"$KIND\"} $3"; fi
}

write_metrics() {
  mkdir -p "$METRICS_DIR" 2>/dev/null || true
  [[ -d "$METRICS_DIR" && -w "$METRICS_DIR" ]] || return 0
  local f="$METRICS_DIR/payment_backup_$KIND.prom" off_on=0
  [[ -n "$OFFSITE_REMOTE" ]] && off_on=1
  {
    metric payment_backup_last_success_timestamp_seconds "Unix time of the last successful backup." "$(state_get last_success)"
    metric payment_backup_last_attempt_timestamp_seconds "Unix time of the last backup attempt." "$(state_get last_attempt)"
    metric payment_backup_last_status "1 if the last backup attempt succeeded, 0 if it failed." "$(state_get last_status)"
    metric payment_backup_last_size_bytes "Size of the last successful backup file." "$(state_get last_size)"
    metric payment_backup_last_items "Files in the last successful archive (kyc only)." "$(state_get last_items)"
    metric payment_backup_offsite_enabled "1 if an off-box copy target is configured." "$off_on"
    metric payment_backup_offsite_last_success_timestamp_seconds "Unix time of the last successful off-box copy." "$(state_get offsite_last_success)"
  } > "$f.tmp.$$" || { rm -f "$f.tmp.$$"; return 0; }
  mv "$f.tmp.$$" "$f"
}

record_attempt() {
  mkdir -p "$STATE_DIR"
  state_set last_attempt "$(date -u +%s)"
}
record_failure() {
  state_set last_status 0 2>/dev/null || true
  write_metrics || true
}
record_success() { # record_success <file> [items]
  state_set last_status 1
  state_set last_success "$(date -u +%s)"
  state_set last_size "$(stat -c %s "$1")"
  state_set last_file "${1#"$BACKUP_ROOT"/}"
  if [[ -n "${2:-}" ]]; then state_set last_items "$2"; fi
  write_metrics
}

die() { log "ERROR: $*"; record_failure; exit 1; }

# The writable volume must be ours (an install upgraded from the root-run
# sidecars needs `chown -R 65532:65532` once — upgrade-hardening.sh does it).
require_writable_root() {
  mkdir -p "$STATE_DIR" 2>/dev/null || true
  if [[ ! -w "$BACKUP_ROOT" || ! -w "$STATE_DIR" ]]; then
    log "ERROR: $BACKUP_ROOT is not writable by uid $(id -u) — run deploy/upgrade-hardening.sh (chowns the backups volume)"
    exit 1
  fi
}

# --- encryption --------------------------------------------------------------
# Sets MODE (age|gpg|none) and EXT (.age|.gpg|"", the file suffix the
# calling script appends) or dies.
# shellcheck disable=SC2034  # EXT is read by db-backup.sh / kyc-backup.sh
select_encryption() {
  case "$ENCRYPTION" in
    none)
      MODE=none; EXT=""
      log "WARNING: BACKUP_ENCRYPTION=none — writing PLAINTEXT backups (dev/test only)"
      return 0 ;;
    age|gpg|auto) ;;
    *) die "BACKUP_ENCRYPTION=$ENCRYPTION must be auto, age, gpg or none" ;;
  esac
  [[ -s "$RECIPIENTS_FILE" ]] \
    || die "no encryption recipients in $RECIPIENTS_FILE (deploy.sh generates an age key; BACKUP_ENCRYPTION=none is for dev only)"
  MODE=$ENCRYPTION
  if [[ "$MODE" == auto ]]; then
    if grep -q 'BEGIN PGP PUBLIC KEY BLOCK' "$RECIPIENTS_FILE"; then
      MODE=gpg
    elif grep -qE '^[[:space:]]*(age1[0-9a-z]+|ssh-(ed25519|rsa) )' "$RECIPIENTS_FILE"; then
      MODE=age
    else
      die "cannot tell whether $RECIPIENTS_FILE holds age recipients or an OpenPGP key"
    fi
  fi
  case "$MODE" in
    age) EXT=.age ;;
    gpg)
      EXT=.gpg
      # A throwaway keyring: --recipient-file needs no import, but gpg wants
      # a home directory. Removed by the calling script's EXIT trap.
      TMP_GNUPGHOME=$(mktemp -d /tmp/gnupg.XXXXXX)
      chmod 700 "$TMP_GNUPGHOME"
      export GNUPGHOME="$TMP_GNUPGHOME" ;;
  esac
}

encrypt() { # stdin -> stdout
  case "$MODE" in
    age)  age --encrypt --recipients-file "$RECIPIENTS_FILE" ;;
    gpg)  gpg --batch --quiet --no-tty --trust-model always \
              --recipient-file "$RECIPIENTS_FILE" --encrypt ;;
    none) cat ;;
  esac
}

# --- off-box copy ------------------------------------------------------------
# Optional. BACKUP_OFFSITE_REMOTE is an rclone remote path (remote:bucket/dir)
# defined in the backup_rclone_conf secret. The file is already encrypted, so
# the target only ever sees ciphertext. Retention there is the bucket's own
# lifecycle policy. A failed copy is logged as an ERROR and shows up as a stale
# payment_backup_offsite_last_success_timestamp_seconds (alerted), but does not
# fail the local backup that already succeeded.
offsite_copy() { # offsite_copy <absolute file> ...
  [[ -n "$OFFSITE_REMOTE" ]] || return 0
  local f rel failed=0
  for f in "$@"; do
    rel=${f#"$BACKUP_ROOT"/}
    if ! rclone --config "$RCLONE_CONFIG_FILE" --retries 3 --low-level-retries 5 \
         copyto "$f" "${OFFSITE_REMOTE%/}/$rel"; then
      failed=1
    fi
  done
  if [[ $failed -eq 0 ]]; then
    state_set offsite_last_success "$(date -u +%s)"
    log "copied off-box to $OFFSITE_REMOTE"
  else
    log "ERROR: off-box copy to $OFFSITE_REMOTE failed (local backup is fine)"
  fi
  write_metrics
}

# prune <dir> <days> <glob>: delete matching files older than <days> days.
prune() {
  [[ -d "$1" ]] || return 0
  find "$1" -maxdepth 1 -type f -name "$3" -mtime +"$2" -print -delete | sed 's/^/  pruned /' >&2 || true
  find "$1" -maxdepth 1 -type l ! -exec test -e {} \; -delete 2>/dev/null || true
}

read_secret_file() { # read_secret_file <path>
  [[ -s "$1" ]] || die "missing or empty secret $1"
  tr -d '\r\n' < "$1"
}
