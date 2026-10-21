#!/usr/bin/env bash
# One-time conversion of an install deployed BEFORE the 2026-10 hardening to
# the role-separated, authenticated layout. Idempotent: safe to re-run. Run it
# from deploy/ after `git pull`, then run ./deploy.sh.
#
#   ./upgrade-hardening.sh
#
# What it does (nothing is deleted; old settings are kept as *.bak-<stamp>):
#   1. app.env: moves a real BIOMETRIC_TEMPLATE_KEY into secrets/ (enrolments
#      stay readable) and comments out every plain secret/URL variable that the
#      compose files now provide as *_FILE (REDIS_URL, NATS_URL, ...).
#   2. secrets/: generates the new files (role passwords, redis/nats auth,
#      trusted public keys, ...) and rewrites database_url, which used to hold
#      the SUPERUSER's credentials, to the payment_app role.
#   3. database: creates payment_owner / payment_app / payment_backup /
#      payment_monitor and transfers every object in schema public from the
#      superuser to payment_owner (postgres/roles.psql +
#      postgres/reassign-ownership.psql). The running app keeps working (it
#      still connects as the superuser until deploy.sh replaces it).
#   4. volumes: chowns `backups` to uid 65532 (the backup sidecars no longer run
#      as root) and caddydata/caddyconfig to uid 10001 (non-root Caddy).
#
# Then ./deploy.sh takes an encrypted pre-deploy dump and rolls out; the new
# server applies migration 0027 (the payment_app grants) as payment_owner.
set -euo pipefail
cd "$(dirname "$0")"
# shellcheck source=lib.sh source-path=SCRIPTDIR
. ./lib.sh

[[ -f .env && -f app.env && -d secrets ]] \
  || die "no existing install here (deploy/.env, app.env, secrets/). A fresh install just runs ./deploy.sh"
stamp=$(date -u +%Y%m%dT%H%M%SZ)

# ---- 0. keep copies of what we are about to change ---------------------------
say "saving app.env -> app.env.bak-$stamp, secrets/ -> secrets/.bak-$stamp/"
(umask 077; cp -p app.env "app.env.bak-$stamp"; cp -p .env ".env.bak-$stamp")
mkdir -p "secrets/.bak-$stamp" && chmod 700 "secrets/.bak-$stamp"
find secrets -maxdepth 1 -type f -exec cp -p {} "secrets/.bak-$stamp/" \;

# ---- 1. app.env --------------------------------------------------------------
bio=$(env_get app.env BIOMETRIC_TEMPLATE_KEY)
if [[ "$bio" =~ ^[0-9a-fA-F]{64}$ ]]; then
  if [[ -s secrets/biometric_template_key && "$(read_secret biometric_template_key)" != "$bio" ]]; then
    die "app.env BIOMETRIC_TEMPLATE_KEY differs from secrets/biometric_template_key — decide which one sealed the enrolments, keep it in the file, delete the line"
  fi
  write_secret biometric_template_key "$bio"
  say "moved BIOMETRIC_TEMPLATE_KEY from app.env into secrets/biometric_template_key"
fi
for v in BIOMETRIC_TEMPLATE_KEY DATABASE_URL MIGRATION_DATABASE_URL JWT_SECRET WORKER_SIGNING_KEY \
         REDIS_URL NATS_URL WORKER_TRUSTED_PUBLIC_KEYS; do
  if grep -qE "^[[:space:]]*$v=" app.env; then
    # The value is dropped, not kept in a comment (it may be a secret).
    sed -i -E "s|^[[:space:]]*$v=.*$|# $v: now ${v}_FILE from deploy/secrets (upgrade-hardening.sh $stamp)|" app.env
    say "app.env: removed $v (now ${v}_FILE, set by the compose files)"
  fi
done
if [[ ":$(setting COMPOSE_FILE):" == *":docker-compose.lan.yml:"* ]] \
   && [[ "$(env_get app.env TRUST_PROXY)" == false ]]; then
  sed -i -E 's|^TRUST_PROXY=false|TRUST_PROXY=true  # LAN API now goes through Caddy (upgrade-hardening.sh)|' app.env
  say "app.env: TRUST_PROXY=true (the LAN overlay's API port is served by Caddy now)"
fi

# ---- 2. secrets ----------------------------------------------------------------
generate_secrets          # new files; database_url now points at payment_app
touch secrets/.roles-v1

# ---- 3. database -----------------------------------------------------------------
# Use the running postgres container if there is one (old or new config);
# otherwise start a throwaway server on the data volume just for this.
volume_exists pgdata || die "volume ${PROJECT}_pgdata not found — nothing to convert (fresh install? run ./deploy.sh)"
ctr=$(docker ps -q --filter "label=com.docker.compose.project=$PROJECT" \
                   --filter "label=com.docker.compose.service=postgres" | head -n1)
temp=""
if [[ -z "$ctr" ]]; then
  img=$(pg_image_ref)
  say "postgres is not running — starting a temporary server on ${PROJECT}_pgdata ($img)"
  temp="payment-upgrade-pg-$$"
  docker run -d --name "$temp" --network none -v "${PROJECT}_pgdata:/var/lib/postgresql/data" \
    --entrypoint docker-entrypoint.sh "$img" postgres >/dev/null
  ctr=$temp
  trap 'docker stop -t 60 "$temp" >/dev/null 2>&1; docker rm "$temp" >/dev/null 2>&1' EXIT
  for _ in $(seq 1 60); do docker exec "$ctr" pg_isready -U payment -d payment >/dev/null 2>&1 && break; sleep 1; done
fi
docker exec "$ctr" pg_isready -U payment -d payment >/dev/null || die "postgres in $ctr is not ready"

say "creating the application roles and transferring schema ownership to payment_owner..."
{
  printf "\\\\set owner_pw '%s'\n"   "$(read_secret pg_owner_password)"
  printf "\\\\set app_pw '%s'\n"     "$(read_secret pg_app_password)"
  printf "\\\\set backup_pw '%s'\n"  "$(read_secret pg_backup_password)"
  printf "\\\\set monitor_pw '%s'\n" "$(read_secret pg_monitor_password)"
  printf "\\\\set dbname '%s'\n"     payment
  cat postgres/roles.psql postgres/reassign-ownership.psql
} | docker exec -i -u postgres "$ctr" psql -X -q -v ON_ERROR_STOP=1 --no-psqlrc -U payment -d payment

# ---- 4. volumes now written by non-root containers -------------------------------
helper=$(pg_image_ref)
chown_volume() { # chown_volume <volume> <uid:gid>
  volume_exists "$1" || return 0
  docker run --rm --user 0:0 --network none --cap-drop ALL --cap-add CHOWN --cap-add DAC_READ_SEARCH \
    --security-opt no-new-privileges -v "${PROJECT}_$1:/v" --entrypoint chown "$helper" -R "$2" /v
  say "chowned volume ${PROJECT}_$1 to $2"
}
chown_volume backups 65532:65532
chown_volume caddydata 10001:10001
chown_volume caddyconfig 10001:10001

cat <<MSG

==> Upgrade prepared. Next:
    1. Check deploy/.env: ADMIN_ALLOW_CIDR is now REQUIRED (admin console + admin API allowlist);
       new optional settings are listed in .env.prod.example (API_REPLICAS, BACKUP_*, SECRETS_GID).
    2. Review deploy/app.env against app.env.example (new: DEPOSIT_DUAL_CONTROL, DEPOSIT_MAX_MINOR,
       BIOMETRIC_IDENTIFY, BIOMETRIC_MAX_ATTEMPTS, VERIFY_FULL_EVERY_SECS; BIOMETRIC_MATCHER=exact is
       refused outside APP_ENV=dev).
    3. ./deploy.sh      (encrypted pre-deploy dump, then rollout; migration 0027 grants payment_app)
    Old values: app.env.bak-$stamp, .env.bak-$stamp, secrets/.bak-$stamp/ (delete once all is well —
    secrets/.bak-$stamp/database_url still holds the superuser URL).
MSG
print_offbox_keys
