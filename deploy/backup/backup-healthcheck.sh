#!/bin/bash
# Container healthcheck for the backup sidecars: healthy only while the last
# SUCCESSFUL backup of this kind is younger than BACKUP_MAX_AGE_SECS (default
# 26 h = a daily schedule plus slack). A sidecar whose scheduler is alive but
# whose backups keep failing is therefore reported unhealthy.
#
#   backup-healthcheck.sh db|kyc
set -euo pipefail
kind=${1:?usage: backup-healthcheck.sh db|kyc}
max=${BACKUP_MAX_AGE_SECS:-93600}
state="${BACKUP_ROOT:-/backups}/.state"

last=$(cat "$state/$kind.last_success" 2>/dev/null || true)
if [[ -z "$last" ]]; then
  echo "no successful $kind backup recorded yet"
  exit 1
fi
age=$(( $(date -u +%s) - last ))
if (( age > max )); then
  echo "last successful $kind backup was ${age}s ago (limit ${max}s); last status: $(cat "$state/$kind.last_status" 2>/dev/null || echo '?')"
  exit 1
fi
echo "last successful $kind backup ${age}s ago"
