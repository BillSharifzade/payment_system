#!/usr/bin/env bash
# One-command deploy for a single server. Idempotent: re-run to roll out.
#
#   ./deploy.sh                     build the images (tag = git SHA, also `latest`), roll out
#   ./deploy.sh --no-build          roll out images that are already present (the LAN box:
#                                   built on the dev box, shipped with docker save | docker load)
#   IMAGE_TAG=<old-sha> ./deploy.sh --no-build     roll BACK to a previous image
#   ./deploy.sh --skip-predeploy-backup            emergency only: no dump before rollout
#
# First run creates deploy/.env (compose settings), deploy/app.env (the app's
# settings) and deploy/secrets/* (generated; 0640, group SECRETS_GID, never in
# .env). An install deployed before the 2026-10 hardening must run
# ./upgrade-hardening.sh once first (this script detects it and stops).
#
# Order of a rollout: preflight checks -> secrets -> images -> data tier up
# (postgres/redis/nats) -> database roles -> encrypted pre-deploy dump of the
# CURRENT database (before any new server can migrate it) -> everything up ->
# wait for health.
set -euo pipefail
cd "$(dirname "$0")"
# shellcheck source=lib.sh source-path=SCRIPTDIR
. ./lib.sh

BUILD=1
PREDEPLOY_BACKUP=1
for arg in "$@"; do
  case "$arg" in
    --no-build) BUILD=0 ;;
    --skip-predeploy-backup) PREDEPLOY_BACKUP=0 ;;
    -h|--help) sed -n '2,17p' "$0"; exit 0 ;;
    *) die "unknown option: $arg (see --help)" ;;
  esac
done

# ---- 1. Settings files ------------------------------------------------------
first_run=0
if [[ ! -f .env ]]; then
  (umask 077; cp .env.prod.example .env)
  first_run=1
  say "created deploy/.env — set SITE_ADDRESS and ADMIN_ALLOW_CIDR (and COMPOSE_FILE for overlays)"
fi
if [[ ! -f app.env ]]; then
  (umask 077; cp app.env.example app.env)
  first_run=1
  say "created deploy/app.env (application settings)"
fi
chmod 600 .env app.env
# Compose reads deploy/.env for interpolation; COMPOSE_FILE inside it selects
# the base file and overlays. An explicit COMPOSE_FILE from the caller wins.
COMPOSE_FILE=$(setting COMPOSE_FILE)
export COMPOSE_FILE="${COMPOSE_FILE:-docker-compose.prod.yml}"
lan=0; monitoring=0; walarchive=0; ha=0
[[ ":$COMPOSE_FILE:" == *":docker-compose.lan.yml:"* ]] && lan=1
[[ ":$COMPOSE_FILE:" == *":docker-compose.monitoring.yml:"* ]] && monitoring=1
[[ ":$COMPOSE_FILE:" == *":docker-compose.wal-archive.yml:"* ]] && walarchive=1
[[ ":$COMPOSE_FILE:" == *":ha/docker-compose.ha.yml:"* ]] && ha=1

# ---- 2. Preflight: refuse unsafe or unconverted configurations --------------
problems=()

allow=$(setting ADMIN_ALLOW_CIDR)
if [[ -z "$allow" ]]; then
  problems+=("ADMIN_ALLOW_CIDR is not set in deploy/.env. It decides who can reach /admin AND the admin API
     (/v1/admin/*, deposits, KYC decisions). Set it to your office/VPN addresses, e.g.
       ADMIN_ALLOW_CIDR=203.0.113.7/32 10.8.0.0/24
     or, to knowingly expose the admin surface to the whole internet: ADMIN_ALLOW_CIDR=0.0.0.0/0 ::/0")
elif [[ " $allow " == *" 0.0.0.0/0 "* || " $allow " == *" ::/0 "* ]]; then
  cat >&2 <<'MSG'

!! WARNING: ADMIN_ALLOW_CIDR includes 0.0.0.0/0 or ::/0 — the admin console and the
!! admin API (deposits, KYC decisions, user blocks, terminals) are reachable from
!! ANYWHERE. Only the admin password stands between the internet and minting money.
!! Restrict it to your office/VPN addresses as soon as possible.

MSG
fi

if [[ $lan -eq 1 ]]; then
  [[ -n "$(setting LAN_BIND_ADDRESS)" ]] \
    || problems+=("LAN overlay: set LAN_BIND_ADDRESS in deploy/.env to the box's LAN address (e.g. 192.168.1.156)")
  site=$(setting SITE_ADDRESS)
  if [[ -n "$site" && "$site" != *8099* ]]; then
    warn "LAN overlay: SITE_ADDRESS=$site has no :8099 site, so the API port answers nothing."
    warn "            Remove SITE_ADDRESS from deploy/.env (default: \"http://:80, http://:8099\")."
  fi
  warn "LAN overlay (docker-compose.lan.yml) is a DEV/STAGING setup: plain HTTP, never real money"
fi
if [[ $monitoring -eq 1 ]]; then
  for v in GRAFANA_PASSWORD ALERT_WEBHOOK_URL ALERT_HEARTBEAT_URL; do
    [[ -n "$(setting "$v")" ]] || problems+=("monitoring overlay: set $v in deploy/.env")
  done
fi
if [[ $walarchive -eq 1 && ! -s secrets/pgbackrest.conf ]]; then
  problems+=("WAL overlay: create secrets/pgbackrest.conf from pgbackrest/pgbackrest.conf.example")
fi
if [[ $ha -eq 1 ]]; then
  [[ $walarchive -eq 0 ]] \
    || problems+=("ha/docker-compose.ha.yml archives WAL itself: remove docker-compose.wal-archive.yml from COMPOSE_FILE")
  [[ -s secrets/pgbackrest.conf ]] \
    || problems+=("HA overlay: create secrets/pgbackrest.conf from ha/pgbackrest.conf.example (WAL archiving is part of the HA stack)")
  # A single-node volume means a ledger that may not have been moved into the
  # cluster yet: deploying would start the app on an EMPTY database.
  if volume_exists pgdata && [[ ! -f secrets/.ha-v1 ]]; then
    problems+=("HA overlay: the single-node database (volume pgdata) has not been moved into the cluster yet.
     Follow deploy/README.md \"Moving to the HA cluster\" (final dump, restore into the leader), then: touch secrets/.ha-v1")
  fi
fi

encryption=$(setting BACKUP_ENCRYPTION)
if [[ "$encryption" == none ]]; then
  if [[ $lan -eq 1 ]]; then
    warn "BACKUP_ENCRYPTION=none: backups are written in PLAINTEXT (allowed on the LAN test box only)"
  else
    problems+=("BACKUP_ENCRYPTION=none is refused for production: backups must be encrypted (leave it unset/auto)")
  fi
fi

# Secrets must come from the compose-provided *_FILE variables. A plain value
# in app.env would silently WIN over the file (env_or_file checks it first).
for v in DATABASE_URL MIGRATION_DATABASE_URL JWT_SECRET WORKER_SIGNING_KEY \
         BIOMETRIC_TEMPLATE_KEY REDIS_URL NATS_URL WORKER_TRUSTED_PUBLIC_KEYS; do
  if [[ -n "$(env_get app.env "$v")" ]]; then
    problems+=("deploy/app.env sets $v directly — it would override the generated secret file.
     Remove the line (./upgrade-hardening.sh does this and moves an existing BIOMETRIC_TEMPLATE_KEY into secrets/).")
  fi
done
app_env_mode=$(env_get app.env APP_ENV)
matcher=$(env_get app.env BIOMETRIC_MATCHER)
if [[ "${matcher:-exact}" == exact && "${app_env_mode:-prod}" != dev ]]; then
  problems+=("deploy/app.env: BIOMETRIC_MATCHER=${matcher:-<unset, i.e. exact>} — the server refuses to boot with the
     exact matcher outside APP_ENV=dev. Set BIOMETRIC_MATCHER=http and BIOMETRIC_MATCHER_URL (see app.env.example:
     without a matcher service only fingerprint operations fail).")
fi
if [[ "$(env_get app.env DEPOSIT_DUAL_CONTROL)" == false && "${app_env_mode:-prod}" != dev ]]; then
  problems+=("deploy/app.env: DEPOSIT_DUAL_CONTROL=false is refused outside APP_ENV=dev")
fi

# An install from before the role split: its database belongs to the superuser
# and its secrets/app.env have the old shape. Convert it once, explicitly.
if volume_exists pgdata; then
  if [[ ! -f secrets/.roles-v1 ]]; then
    problems+=("existing install from before the 2026-10 hardening (database owned by the superuser, old secrets).
     Run ./upgrade-hardening.sh once, then ./deploy.sh again.")
  fi
else
  mkdir -p secrets && chmod 700 secrets
  touch secrets/.roles-v1   # fresh install: born with the role layout
fi

if [[ ${#problems[@]} -gt 0 ]]; then
  echo >&2
  for p in "${problems[@]}"; do echo "!!  $p" >&2; echo >&2; done
  die "refusing to deploy (${#problems[@]} problem(s) above)"
fi

# ---- 3. Secrets as files (generated once; derived files kept in sync) -------
generate_secrets

# ---- 4. Image tag -----------------------------------------------------------
if [[ -z "${IMAGE_TAG:-}" ]]; then
  if git -C .. rev-parse --short HEAD >/dev/null 2>&1; then
    IMAGE_TAG=$(git -C .. rev-parse --short HEAD)
    if [[ -n "$(git -C .. status --porcelain 2>/dev/null)" ]]; then
      IMAGE_TAG="${IMAGE_TAG}-dirty"
    fi
  else
    IMAGE_TAG=latest
  fi
fi
export IMAGE_TAG
say "IMAGE_TAG=$IMAGE_TAG  COMPOSE_FILE=$COMPOSE_FILE"

# ---- 5. Build (or check the shipped images) ---------------------------------
images=(payment-system payment-console payment-backup)
[[ $walarchive -eq 1 ]] && images+=(payment-postgres)
[[ $ha -eq 1 ]] && images+=(payment-patroni)
if [[ $BUILD -eq 1 ]]; then
  say "building images (cargo-chef caches the dependency layer)..."
  docker compose build
  for img in "${images[@]}"; do docker tag "$img:$IMAGE_TAG" "$img:latest"; done
else
  for img in "${images[@]}"; do
    docker image inspect "$img:$IMAGE_TAG" >/dev/null 2>&1 \
      || die "image $img:$IMAGE_TAG not present (docker load it, or drop --no-build)"
  done
fi

# ---- 6. Backup encryption key + secret file permissions ---------------------
ensure_backup_key "payment-backup:$IMAGE_TAG"
apply_secret_perms "payment-backup:$IMAGE_TAG"
export SECRETS_GID

# ---- 7. API replicas -> one Caddy upstream per container ---------------------
API_REPLICAS=$(setting API_REPLICAS); API_REPLICAS=${API_REPLICAS:-1}
[[ "$API_REPLICAS" =~ ^[1-9][0-9]?$ ]] || die "API_REPLICAS=$API_REPLICAS must be 1..99"
API_UPSTREAMS=""
for i in $(seq 1 "$API_REPLICAS"); do API_UPSTREAMS+="${API_UPSTREAMS:+ }${PROJECT}-payment-server-$i:8080"; done
export API_REPLICAS API_UPSTREAMS

# ---- 8. Data tier, roles, pre-deploy dump -----------------------------------
say "starting postgres, redis, nats..."
docker compose up -d --wait --wait-timeout 180 postgres redis nats
# With the HA overlay `postgres` is HAProxy: superuser work runs on the leading member.
dbsvc=postgres
if [[ $ha -eq 1 ]]; then
  dbsvc=$(ha/ha-leader.sh) || die "no database member is leading (docker compose exec pg-1 patronictl list)"
fi
say "database roles (postgres/10-roles.sh on $dbsvc)..."
docker compose exec -T "$dbsvc" bash /docker-entrypoint-initdb.d/10-roles.sh

psql_su() { docker compose exec -T "$dbsvc" psql -X -qtA -v ON_ERROR_STOP=1 -U payment -d payment "$@"; }
# (assigned first: a failing query must stop the deploy, not read as "empty")
has_schema=$(psql_su -c "SELECT to_regclass('public._sqlx_migrations') IS NOT NULL") \
  || die "cannot query the database (docker compose logs postgres)"
if [[ "$has_schema" == t ]]; then
  foreign=$(psql_su -c "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                         WHERE n.nspname = 'public' AND c.relkind IN ('r','p','S')
                           AND pg_get_userbyid(c.relowner) <> 'payment_owner'")
  [[ "$foreign" == 0 ]] \
    || die "$foreign table(s)/sequence(s) in schema public are not owned by payment_owner — run ./upgrade-hardening.sh"
  if [[ $PREDEPLOY_BACKUP -eq 1 ]]; then
    say "pre-deploy dump of the current database (before the new server can migrate it)..."
    docker compose run --rm --no-deps -T --entrypoint /usr/local/bin/db-backup.sh \
      postgres-backup --label "pre-deploy-$IMAGE_TAG" \
      || die "pre-deploy backup FAILED — nothing was rolled out. Fix it (docker compose logs postgres-backup), or
    re-run with --skip-predeploy-backup if you accept deploying without a fresh restore point."
  else
    warn "--skip-predeploy-backup: rolling out WITHOUT a fresh restore point"
  fi
else
  say "empty database (first deploy): no pre-deploy dump needed"
fi

# ---- 9. Roll out ------------------------------------------------------------
say "starting the stack..."
docker compose up -d --remove-orphans

# ---- 10. Wait for health ----------------------------------------------------
wait_healthy() { # wait_healthy <seconds> <service>...
  local deadline=$((SECONDS + $1)) svc id st bad; shift
  while :; do
    bad=""
    for svc in "$@"; do
      local ids; ids=$(docker compose ps -q "$svc")
      [[ -n "$ids" ]] || { bad+=" $svc(not running)"; continue; }
      for id in $ids; do
        st=$(docker inspect -f '{{.State.Status}}/{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' "$id")
        [[ "$st" == running/healthy || "$st" == running/none ]] || bad+=" $svc($st)"
      done
    done
    [[ -z "$bad" ]] && return 0
    if (( SECONDS >= deadline )); then echo "$bad"; return 1; fi
    sleep 3
  done
}
say "waiting for payment-server (x$API_REPLICAS), payment-workers and caddy to be healthy..."
if ! bad=$(wait_healthy "${DEPLOY_WAIT_SECS:-300}" payment-server payment-workers caddy); then
  docker compose ps >&2
  die "not healthy in time:$bad — inspect: docker compose logs --tail=100 payment-server payment-workers caddy"
fi
docker compose ps
for svc in postgres-backup kyc-backup; do
  if ! wait_healthy 1 "$svc" >/dev/null; then
    warn "$svc is not healthy yet (its first backup may still be running): docker compose logs $svc"
  fi
done

if [[ $first_run -eq 1 || ${#GENERATED[@]} -gt 0 ]]; then
  [[ ${#GENERATED[@]} -gt 0 ]] && say "new secrets this run: ${GENERATED[*]}"
  print_offbox_keys
fi
cat <<MSG

==> Deployed $IMAGE_TAG.
    Roll back:   IMAGE_TAG=<previous-sha> ./deploy.sh --no-build
    Images kept: docker images 'payment-*'
    Logs:        docker compose logs -f payment-server payment-workers
    Drill:       ./restore-drill.sh   (monthly; proves the newest backup restores and verifies)
MSG
