#!/bin/bash
# Creates (or updates) the application's database roles from secret files.
# Idempotent; safe to run any number of times.
#
#   * First initialisation of a fresh data volume: the official postgres
#     entrypoint runs every script in /docker-entrypoint-initdb.d/ (this file is
#     mounted there by the compose files) over the local socket.
#   * Every deploy: deploy.sh runs it again through
#       docker compose exec -T postgres bash /docker-entrypoint-initdb.d/10-roles.sh
#     so a rotated password file takes effect and a missing role is recreated.
#
# Roles (see migrations/0027_db_role_grants.sql for their table privileges):
#   payment          bootstrap superuser (POSTGRES_USER) — init + emergencies only
#   payment_owner    owns the database and the schema; runs the migrations
#   payment_app      runtime role of payment-server + payment-workers
#   payment_backup   read-only (pg_read_all_data) for the dump sidecar
#   payment_monitor  pg_monitor for postgres-exporter
#
# Passwords are read here, inside the container, from /run/secrets/* and handed
# to psql through its environment (\getenv), so they never appear in a process
# argument list or a log line.
# The entrypoint SOURCES a non-executable *.sh (e.g. a checkout without exec
# bits); run in a child shell then, so our shell options never leak into it.
if [[ "${BASH_SOURCE[0]}" != "$0" ]]; then
  bash "${BASH_SOURCE[0]}"
  return $?
fi
set -euo pipefail

db=${POSTGRES_DB:-payment}
su=${POSTGRES_USER:-payment}
secrets=${SECRETS_DIR:-/run/secrets}   # overridable for tests only

read_secret() { # read_secret <file name under $secrets>
  local f="$secrets/$1"
  if [[ ! -s "$f" ]]; then
    echo "10-roles.sh: missing or empty secret $f" >&2
    exit 1
  fi
  local v
  v=$(tr -d '\r\n' < "$f")
  # deploy.sh generates hex; anything else could break the URL-shaped secrets.
  if [[ ! "$v" =~ ^[A-Za-z0-9._~-]{16,}$ ]]; then
    echo "10-roles.sh: $f must be >= 16 URL-safe characters" >&2
    exit 1
  fi
  printf '%s' "$v"
}

PAYMENT_OWNER_PASSWORD=$(read_secret pg_owner_password)
PAYMENT_APP_PASSWORD=$(read_secret pg_app_password)
PAYMENT_BACKUP_PASSWORD=$(read_secret pg_backup_password)
PAYMENT_MONITOR_PASSWORD=$(read_secret pg_monitor_password)
export PAYMENT_OWNER_PASSWORD PAYMENT_APP_PASSWORD PAYMENT_BACKUP_PASSWORD PAYMENT_MONITOR_PASSWORD
export PAYMENT_DB="$db"

# roles.psql sits next to this file (both are mounted into initdb.d).
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
{
  printf '%s\n' '\getenv owner_pw PAYMENT_OWNER_PASSWORD' '\getenv app_pw PAYMENT_APP_PASSWORD' \
    '\getenv backup_pw PAYMENT_BACKUP_PASSWORD' '\getenv monitor_pw PAYMENT_MONITOR_PASSWORD' \
    '\getenv dbname PAYMENT_DB'
  cat "$here/roles.psql"
} | psql -v ON_ERROR_STOP=1 --no-psqlrc -q -U "$su" -d "$db"

echo "10-roles.sh: roles payment_owner, payment_app, payment_backup, payment_monitor are in place (database $db)"
