#!/bin/bash
# Scheduler for the backup sidecars (the container's entrypoint). No cron, no
# root: sleeps until the next BACKUP_AT (HH:MM, UTC) and runs the backup.
#
#   backup-loop.sh db     runs db-backup.sh
#   backup-loop.sh kyc    runs kyc-backup.sh
#
# On start it catches up: if the last SUCCESS is older than 24 h (or there has
# never been one) a backup runs immediately, so a fresh install or a box that
# was off at backup time is covered at once — but a plain restart does not
# produce a redundant dump. A failed run is retried after BACKUP_RETRY_SECS
# (default 1 h) instead of waiting a whole day. The container's healthcheck
# (backup-healthcheck.sh) reports the age of the last success.
set -euo pipefail
KIND=${1:?usage: backup-loop.sh db|kyc}
case "$KIND" in
  db|kyc) ;;
  *) echo "usage: backup-loop.sh db|kyc" >&2; exit 2 ;;
esac
# shellcheck source=backup-lib.sh source-path=SCRIPTDIR
. "$(dirname "$0")/backup-lib.sh"

AT=${BACKUP_AT:-02:00}
RETRY_SECS=${BACKUP_RETRY_SECS:-3600}
DAY=86400
[[ "$AT" =~ ^([01][0-9]|2[0-3]):[0-5][0-9]$ ]] || { log "ERROR: BACKUP_AT=$AT must be HH:MM (UTC)"; exit 2; }
script="$(dirname "$0")/$KIND-backup.sh"

sleeper=""
on_term() {
  log "stopping"
  [[ -n "$sleeper" ]] && kill "$sleeper" 2>/dev/null
  exit 0
}
trap on_term TERM INT

next_scheduled() { # epoch of the next AT strictly after now
  local now today t
  now=$(date -u +%s)
  today=$(date -u +%Y-%m-%d)
  t=$(date -u -d "$today $AT:00" +%s)
  (( t > now )) || t=$((t + DAY))
  echo "$t"
}

run_backup() {
  if "$script"; then
    return 0
  fi
  log "ERROR: backup run failed — retrying in ${RETRY_SECS}s"
  return 1
}

mkdir -p "$STATE_DIR" 2>/dev/null || true
write_metrics || true
last=$(state_get last_success)
now=$(date -u +%s)
if [[ -z "$last" ]] || (( now - last >= DAY )); then
  log "no successful backup in the last 24 h — running one now"
  if run_backup; then next=$(next_scheduled); else next=$((now + RETRY_SECS)); fi
else
  next=$(next_scheduled)
fi

while :; do
  now=$(date -u +%s)
  wait_s=$((next - now))
  if (( wait_s > 0 )); then
    log "next backup at $(date -u -d "@$next" +%Y-%m-%dT%H:%M:%SZ)"
    sleep "$wait_s" &
    sleeper=$!
    wait "$sleeper" || true
    sleeper=""
  fi
  if run_backup; then
    next=$(next_scheduled)
  else
    sched=$(next_scheduled)
    retry=$(( $(date -u +%s) + RETRY_SECS ))
    next=$(( retry < sched ? retry : sched ))
  fi
done
