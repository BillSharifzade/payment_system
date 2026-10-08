#!/usr/bin/env bash
# Generates the secrets the HA overlay adds to deploy/secrets/ (idempotent: an
# existing non-empty file is kept). Run from deploy/ once, before ./deploy.sh:
#
#   ha/init-secrets.sh                      the four passwords below
#   ha/init-secrets.sh --pgbackrest-posix   ... plus secrets/pgbackrest.conf with a local
#                                           (posix) repository on the shared pgbackrest-repo
#                                           volume — CI and drills only; production copies
#                                           ha/pgbackrest.conf.example (S3) instead
#
#   pg_replication_password   Postgres role `replicator` (streaming replication)
#   patroni_api_password      Patroni REST API user `patroni` (switchover, restart, config)
#   etcd_root_password        etcd `root` (emergencies only; created by etcd-auth)
#   etcd_patroni_password     etcd `patroni`: may read/write only Patroni's /service/ keys
#
# deploy.sh applies the usual permissions (0640, group SECRETS_GID) on its next run.
set -euo pipefail
dir=${HA_SECRETS_DIR:-secrets}
posix=0
for arg in "$@"; do
  case "$arg" in
    --pgbackrest-posix) posix=1 ;;
    *) echo "usage: $0 [--pgbackrest-posix]" >&2; exit 2 ;;
  esac
done
mkdir -p "$dir" && chmod 700 "$dir"
umask 077
for s in pg_replication_password patroni_api_password etcd_root_password etcd_patroni_password; do
  if [[ ! -s "$dir/$s" ]]; then
    openssl rand -hex 24 > "$dir/$s"
    echo "generated $dir/$s"
  fi
done
if [[ $posix -eq 1 && ! -s "$dir/pgbackrest.conf" ]]; then
  cat > "$dir/pgbackrest.conf" <<'CONF'
# Local (posix) pgBackRest repository on the pgbackrest-repo volume that every
# member mounts — for CI and failover drills. Production: ha/pgbackrest.conf.example.
[global]
repo1-path=/var/lib/pgbackrest
repo1-retention-full=2
start-fast=y
log-level-console=info
log-level-file=off

[payment]
pg1-path=/var/lib/postgresql/data/pgdata
pg1-port=5432
pg1-socket-path=/run/postgresql
pg1-user=payment
CONF
  echo "generated $dir/pgbackrest.conf (posix repository)"
fi
