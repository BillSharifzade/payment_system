#!/usr/bin/env bash
# The deploy/ha cluster as plain local processes (no Docker): 3 etcd members,
# 3 Patroni-managed Postgres 16 nodes and HAProxy, from the SAME files the
# compose overlay uses (patroni.yml, render-config.py, post-bootstrap.sh,
# etcd-auth.py, haproxy.cfg, deploy/postgres/10-roles.sh). Used by
# scripts/ha-failover-drill.sh --mode local; handy on its own for poking at
# failover.
#
#   scripts/ha-local.sh up                      fresh cluster; returns once a leader and
#                                               a quorum standby exist and HAProxy routes
#   scripts/ha-local.sh down                    kill everything, delete HA_WORKDIR
#   scripts/ha-local.sh status                  patronictl list
#   scripts/ha-local.sh kill|start|freeze|thaw N   SIGKILL / restart / SIGSTOP / SIGCONT
#                                               node N's Patroni AND all its Postgres processes
#   scripts/ha-local.sh patronictl ARGS...      patronictl against the cluster
#   scripts/ha-local.sh as-node N CMD...        CMD as HA_OS_USER with node N's environment
#                                               (e.g. pgbackrest --stanza=payment check)
#
# Runs as root; etcd, Patroni (hence Postgres) and HAProxy run as HA_OS_USER.
# Listens on 127.0.0.1 only. Ports: HAProxy rw 5450 / ro 5454 / admin 7450,
# Postgres 5451-5453, Patroni REST 8451-8453, etcd client 2451-2453 / peer
# 2461-2463 / metrics 2471-2473.
#
# Tools (env): HA_WORKDIR (/tmp/ha-drill), HA_OS_USER (postgres), PATRONI_BIN
# (patroni on PATH; its directory must also hold patronictl and a python3 with
# PyYAML, i.e. the image's venv: `python3 -m venv V && V/bin/pip install
# --require-hashes -r deploy/ha/requirements.txt`, needs libpq5),
# ETCD_BIN (etcd on PATH), HAPROXY_BIN, PG_BIN (/usr/lib/postgresql/16/bin);
# pgbackrest must be on PATH.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
W=${HA_WORKDIR:-/tmp/ha-drill}
OS_USER=${HA_OS_USER:-postgres}
PG_BIN=${PG_BIN:-/usr/lib/postgresql/16/bin}
PATRONI_BIN=${PATRONI_BIN:-$(command -v patroni || true)}
ETCD_BIN=${ETCD_BIN:-$(command -v etcd || true)}
HAPROXY_BIN=${HAPROXY_BIN:-$(command -v haproxy || true)}
NODES=(1 2 3)

die() { echo "ha-local: $*" >&2; exit 1; }
log() { echo "ha-local: $*" >&2; }
as_user() { setpriv --reuid="$OS_USER" --regid="$OS_USER" --init-groups "$@"; }
etcd_hosts() { echo 127.0.0.1:2451,127.0.0.1:2452,127.0.0.1:2453; }

# start_bg <name> <cmd...>: detached, as OS_USER, output to log/<name>.log, pid in run/<name>.pid
# (a plain command, not the as_user function: a backgrounded function would
# leave $! pointing at a bash subshell instead of the process itself)
start_bg() {
  local name=$1; shift
  setpriv --reuid="$OS_USER" --regid="$OS_USER" --init-groups setsid "$@" >>"$W/log/$name.log" 2>&1 < /dev/null &
  echo $! > "$W/run/$name.pid"
}

wait_for() { # wait_for <seconds> <description> <cmd...>
  local limit=$1 what=$2 deadline=$((SECONDS + $1)); shift 2
  until "$@" >/dev/null 2>&1; do
    (( SECONDS < deadline )) || die "timed out after ${limit}s waiting for $what"
    sleep 0.5
  done
}

port_free() { ! (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }

patroni_env() { # patroni_env <n>: the environment of node n's Patroni (inherited by Postgres,
                # archive_command and post_bootstrap)
  local n=$1
  echo "PATH=$W/bin:$PG_BIN:$(dirname "$PATRONI_BIN"):/usr/local/bin:/usr/bin:/bin" \
       "HOME=$W/pg-$n" "PGHOST=/var/run/postgresql" "PGPORT=545$n" \
       "HA_ROLES_SCRIPT=$W/conf/10-roles.sh" "SECRETS_DIR=$W/secrets" \
       "PGBACKREST_CONFIG=$W/conf/pgbackrest.conf" "PGBACKREST_PG1_PATH=$W/pg-$n/data" "PGBACKREST_PG1_PORT=545$n" \
       "PYTHONDONTWRITEBYTECODE=1"
}

start_node() {
  local n=$1 env
  read -r -a env <<< "$(patroni_env "$n")"
  start_bg "pg-$n" env -i "${env[@]}" "$PATRONI_BIN" "$W/pg-$n/patroni.yml"
}

descendants() { # descendants <pid>: the pid and every process below it
  local c
  echo "$1"
  for c in $(pgrep -P "$1" || true); do descendants "$c"; done
}

# Every process of node n: its Patroni and descendants, plus the postmaster
# (which Patroni detaches into its own session) and its children.
node_pids() {
  local n=$1 p
  {
    if [[ -s "$W/run/pg-$n.pid" ]]; then
      p=$(cat "$W/run/pg-$n.pid")
      ! kill -0 "$p" 2>/dev/null || descendants "$p"
    fi
    if [[ -s "$W/pg-$n/data/postmaster.pid" ]]; then
      p=$(head -n1 "$W/pg-$n/data/postmaster.pid")
      ! kill -0 "$p" 2>/dev/null || descendants "$p"
    fi
  } | sort -un
}

api_get() { curl -fsS -m 2 "http://127.0.0.1:845$1$2"; }

cmd_up() {
  [[ $EUID -eq 0 ]] || die "run as root (Postgres runs as $OS_USER)"
  [[ -x "$PATRONI_BIN" ]] || die "patroni not found (set PATRONI_BIN)"
  [[ -x "$ETCD_BIN" ]] || die "etcd not found (set ETCD_BIN)"
  [[ -x "$HAPROXY_BIN" ]] || die "haproxy not found (set HAPROXY_BIN)"
  [[ -x "$PG_BIN/postgres" ]] || die "no Postgres binaries in $PG_BIN"
  command -v pgbackrest >/dev/null || die "pgbackrest not on PATH"
  local py; py="$(dirname "$PATRONI_BIN")/python3"
  [[ -x "$py" ]] || die "$py not found (PATRONI_BIN must live in a venv)"
  [[ ! -e "$W" ]] || die "$W exists: scripts/ha-local.sh down first"
  for p in 5450 5451 5452 5453 5454 7450 8451 8452 8453 2451 2452 2453 2461 2462 2463 2471 2472 2473; do
    port_free "$p" || die "port $p is in use"
  done

  mkdir -p "$W"/{run,log,bin,conf,secrets,pgbackrest-repo} "$W"/pg-{1,2,3} "$W"/etcd-{1,2,3}
  local s
  for s in postgres_password pg_owner_password pg_app_password pg_backup_password pg_monitor_password \
           pg_replication_password patroni_api_password etcd_root_password etcd_patroni_password; do
    openssl rand -hex 24 > "$W/secrets/$s"
  done
  cp "$ROOT/deploy/postgres/10-roles.sh" "$ROOT/deploy/postgres/roles.psql" "$W/conf/"
  install -m 0755 "$ROOT/deploy/ha/post-bootstrap.sh" "$W/bin/ha-post-bootstrap"
  install -m 0755 "$ROOT/deploy/ha/on-role-change.sh" "$W/bin/ha-on-role-change"
  # Same settings as deploy/ha/pgbackrest.conf.example, with a local
  # repository instead of S3; pg1-path/port come per node from the environment.
  cat > "$W/conf/pgbackrest.conf" <<CONF
[global]
repo1-path=$W/pgbackrest-repo
repo1-retention-full=2
start-fast=y
log-level-console=info
log-level-file=off
lock-path=$W/run/pgbackrest

[payment]
pg1-socket-path=/var/run/postgresql
pg1-user=payment
CONF
  chown -R "$OS_USER:" "$W"
  chmod 700 "$W" "$W/secrets"
  chmod 600 "$W"/secrets/*

  log "etcd x3"
  local n cluster="etcd-1=http://127.0.0.1:2461,etcd-2=http://127.0.0.1:2462,etcd-3=http://127.0.0.1:2463"
  for n in "${NODES[@]}"; do
    start_bg "etcd-$n" "$ETCD_BIN" --name "etcd-$n" --data-dir "$W/etcd-$n" \
      --listen-client-urls "http://127.0.0.1:245$n" --advertise-client-urls "http://127.0.0.1:245$n" \
      --listen-peer-urls "http://127.0.0.1:246$n" --initial-advertise-peer-urls "http://127.0.0.1:246$n" \
      --listen-metrics-urls "http://127.0.0.1:247$n" --initial-cluster "$cluster" \
      --initial-cluster-state new --initial-cluster-token payment-ha-local --auto-compaction-retention 1
  done
  for n in "${NODES[@]}"; do wait_for 60 "etcd-$n" curl -fsS "http://127.0.0.1:247$n/health"; done
  HA_ETCD_HOSTS=$(etcd_hosts) HA_SECRETS_DIR="$W/secrets" python3 -I "$ROOT/deploy/ha/etcd-auth.py" >&2

  log "Patroni x3"
  for n in "${NODES[@]}"; do
    as_user env HA_NODE_NAME="pg-$n" HA_PG_LISTEN="127.0.0.1:545$n" HA_PG_CONNECT="127.0.0.1:545$n" \
      HA_RESTAPI_LISTEN="127.0.0.1:845$n" HA_RESTAPI_CONNECT="127.0.0.1:845$n" \
      HA_PG_DATA_DIR="$W/pg-$n/data" HA_ETCD_HOSTS="$(etcd_hosts)" HA_SECRETS_DIR="$W/secrets" \
      "$py" "$ROOT/deploy/ha/render-config.py" "$ROOT/deploy/ha/patroni.yml" "$W/pg-$n/patroni.yml"
    start_node "$n"
  done

  log "HAProxy"
  start_bg haproxy env HA_BIND_RW=127.0.0.1:5450 HA_BIND_RO=127.0.0.1:5454 HA_BIND_ADMIN=127.0.0.1:7450 \
    HA_PG1=127.0.0.1:5451 HA_PG2=127.0.0.1:5452 HA_PG3=127.0.0.1:5453 \
    HA_API1_PORT=8451 HA_API2_PORT=8452 HA_API3_PORT=8453 \
    "$HAPROXY_BIN" -db -f "$ROOT/deploy/ha/haproxy.cfg"

  wait_for 180 "a leader behind HAProxy" curl -fsS "http://127.0.0.1:7450/health"
  wait_for 180 "a quorum standby" quorum_ready
  log "up: $(cmd_status | grep -c streaming) streaming replicas; leader $(leader)"
}

leader() {
  local n
  for n in "${NODES[@]}"; do
    if api_get "$n" /primary >/dev/null 2>&1; then echo "$n"; return 0; fi
  done
  return 1
}

quorum_ready() {
  local n
  for n in "${NODES[@]}"; do
    api_get "$n" /patroni 2>/dev/null | grep -q '"quorum_standby": *true\|"sync_standby": *true' && return 0
  done
  return 1
}

cmd_patronictl() { "$(dirname "$PATRONI_BIN")/patronictl" -c "$W/pg-1/patroni.yml" "$@"; }
cmd_status() { cmd_patronictl list; }

cmd_kill() {
  local n=$1 pids
  mapfile -t pids < <(node_pids "$n")
  [[ ${#pids[@]} -gt 0 ]] || die "node $n has no running processes"
  kill -KILL "${pids[@]}" 2>/dev/null || true
  log "node $n: SIGKILL ${#pids[@]} processes"
}

cmd_freeze() {
  local n=$1 pids
  mapfile -t pids < <(node_pids "$n")
  [[ ${#pids[@]} -gt 0 ]] || die "node $n has no running processes"
  printf '%s\n' "${pids[@]}" > "$W/run/pg-$n.frozen"
  kill -STOP "${pids[@]}"
  log "node $n: SIGSTOP ${#pids[@]} processes"
}

cmd_thaw() {
  local n=$1 pids
  [[ -s "$W/run/pg-$n.frozen" ]] || die "node $n is not frozen"
  mapfile -t pids < "$W/run/pg-$n.frozen"
  kill -CONT "${pids[@]}" 2>/dev/null || true
  rm -f "$W/run/pg-$n.frozen"
  log "node $n: SIGCONT ${#pids[@]} processes"
}

cmd_start() {
  local n=$1
  [[ -z "$(node_pids "$n")" ]] || die "node $n is still running"
  start_node "$n"
  log "node $n: Patroni started"
}

cmd_down() {
  [[ -d "$W" ]] || return 0
  local f pids=() n
  for n in "${NODES[@]}"; do
    [[ -f "$W/run/pg-$n.frozen" ]] && cmd_thaw "$n"
    mapfile -t -O "${#pids[@]}" pids < <(node_pids "$n")
  done
  for f in "$W"/run/*.pid; do
    [[ -s "$f" ]] && pids+=("$(cat "$f")")
  done
  [[ ${#pids[@]} -eq 0 ]] || kill -KILL "${pids[@]}" 2>/dev/null || true
  sleep 0.5
  rm -rf "$W"
  log "down"
}

cmd=${1:-}
shift || true
case "$cmd" in
  up) cmd_up ;;
  down) cmd_down ;;
  status) cmd_status ;;
  leader) leader ;;
  patronictl) cmd_patronictl "$@" ;;
  as-node)
    [[ "${1:-}" =~ ^[123]$ && $# -ge 2 ]] || die "usage: $0 as-node 1|2|3 CMD..."
    read -r -a env <<< "$(patroni_env "$1")"
    shift
    as_user env -i "${env[@]}" "$@" ;;
  kill|start|freeze|thaw)
    [[ "${1:-}" =~ ^[123]$ ]] || die "usage: $0 $cmd 1|2|3"
    "cmd_$cmd" "$1" ;;
  *) sed -n '2,28p' "$0"; exit 2 ;;
esac
