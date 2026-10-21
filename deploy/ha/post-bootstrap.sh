#!/bin/bash
# Patroni bootstrap.post_bootstrap (installed as `ha-post-bootstrap`): runs ONCE,
# on the node that initialises the cluster, against the brand-new primary.
# Patroni passes a superuser connection string as $1 and sets PGPASSFILE and
# PGOPTIONS (synchronous_commit=local: no standby exists yet to acknowledge).
#
#   1. CREATE DATABASE payment (the official image's POSTGRES_DB does this in
#      the single-node stack);
#   2. the application roles, exactly as in the single-node stack:
#      deploy/postgres/10-roles.sh (reads the pg_*_password secret files);
#   3. the pgBackRest stanza, so archive_command starts succeeding at once.
# Replicas receive all of it through replication.
set -euo pipefail
conn=${1:?usage: ha-post-bootstrap <superuser connection string>}
db=${POSTGRES_DB:-payment}

psql -X -q -v ON_ERROR_STOP=1 -v db="$db" "$conn" <<'SQL'
SELECT format('CREATE DATABASE %I', :'db')
WHERE NOT EXISTS (SELECT 1 FROM pg_database WHERE datname = :'db')
\gexec
SQL

bash "${HA_ROLES_SCRIPT:-/docker-entrypoint-initdb.d/10-roles.sh}"

pgbackrest --stanza=payment --log-level-console=warn stanza-create
echo "ha-post-bootstrap: database $db, roles and pgBackRest stanza ready"
