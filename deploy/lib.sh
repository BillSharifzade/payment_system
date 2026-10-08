# shellcheck shell=bash
# Helpers shared by deploy.sh and upgrade-hardening.sh (sourced from deploy/).
#
# Secrets live as files under deploy/secrets/ (directory 0700). Each file is
# 0640, owned by the deploy user, group SECRETS_GID (default 10500): every
# container that is handed a secret runs with `group_add: [SECRETS_GID]`, so it
# can read exactly the files mounted into it, while nobody else on the host can
# even list the directory. (Compose bind-mounts file secrets with their host
# owner and mode; a plain 0600 file would be unreadable by the containers'
# non-root users.) backup_age_identity is the exception: 0600, never mounted.

PROJECT=${COMPOSE_PROJECT_NAME:-payment}
SECRETS_GID=${SECRETS_GID:-10500}
GENERATED=()

say()  { echo "==> $*"; }
warn() { echo "!!  $*" >&2; }
die()  { echo "!!  $*" >&2; exit 1; }

# env_get <file> <KEY>: the last KEY=value in a dotenv file, surrounding quotes
# stripped; empty when unset or commented out.
env_get() {
  [[ -f "$1" ]] || return 0
  sed -n "s/^[[:space:]]*$2=//p" "$1" | tail -n1 \
    | sed -e 's/[[:space:]]*$//' -e 's/^"\(.*\)"$/\1/' -e "s/^'\(.*\)'\$/\1/"
}

# The setting as compose will see it: the caller's environment wins over .env.
setting() { # setting <KEY>
  local v=${!1:-}
  [[ -n "$v" ]] && { printf '%s' "$v"; return 0; }
  env_get .env "$1"
}

# --- secret files ---------------------------------------------------------------
secret_path() { printf 'secrets/%s' "$1"; }
read_secret() { tr -d '\r\n' < "secrets/$1"; }

write_secret() { # write_secret <name> <content>: atomic, never world-readable
  ( umask 077
    printf '%s' "$2" > "secrets/.$1.tmp"
    mv -f "secrets/.$1.tmp" "secrets/$1" )
}

gen_secret() { # gen_secret <name> <random bytes>: hex, only when missing/empty
  [[ -s "secrets/$1" ]] && return 0
  write_secret "$1" "$(openssl rand -hex "$2")"
  GENERATED+=("$1")
  say "generated secrets/$1"
}

# sync_secret <name> <content>: (re)write a derived file when it differs.
sync_secret() {
  if [[ ! -f "secrets/$1" ]] || [[ "$(cat "secrets/$1")" != "$2" ]]; then
    write_secret "$1" "$2"
    say "wrote secrets/$1"
  fi
}

# Ed25519 public key (hex) of a 32-byte seed (hex) — the same derivation the
# workers' signer uses. OpenSSL reads the seed as a PKCS#8 DER private key.
ed25519_public_hex() {
  local seed=$1 der
  [[ "$seed" =~ ^[0-9a-fA-F]{64}$ ]] || return 1
  der=$(printf '302e020100300506032b657004220420%s' "$seed" | sed 's/../\\x&/g')
  # shellcheck disable=SC2059  # the escapes ARE the data
  printf "$der" | openssl pkey -inform DER -pubout -outform DER \
    | od -An -tx1 -v | tr -d ' \n' | tail -c 64
}

generate_secrets() {
  mkdir -p secrets && chmod 700 secrets

  # Database: bootstrap superuser + the four application roles.
  gen_secret postgres_password    24
  gen_secret pg_owner_password    24
  gen_secret pg_app_password      24
  gen_secret pg_backup_password   24
  gen_secret pg_monitor_password  24
  # Connection URLs follow the password files (edit/delete a password file and
  # redeploy to rotate: 10-roles.sh applies it to the role on the same run).
  sync_secret database_url           "postgres://payment_app:$(read_secret pg_app_password)@postgres:5432/payment"
  sync_secret migration_database_url "postgres://payment_owner:$(read_secret pg_owner_password)@postgres:5432/payment"

  # Application keys.
  gen_secret jwt_secret              32
  gen_secret worker_signing_key      32
  gen_secret biometric_template_key  32

  # The workers' verifier (and restore-drill.sh) trust these Ed25519 public
  # keys, comma-separated. The current signing key's public key is always in
  # it; when rotating the signing key, KEEP the old public key in this list or
  # old checkpoints stop verifying.
  local pub keys
  pub=$(ed25519_public_hex "$(read_secret worker_signing_key)") \
    || die "secrets/worker_signing_key must be 64 hex characters"
  keys=$(cat secrets/worker_trusted_public_keys 2>/dev/null | tr -d ' \r\n' || true)
  if [[ ",$keys," != *",$pub,"* ]]; then
    keys=${keys:+$keys,}$pub
    write_secret worker_trusted_public_keys "$keys"
    say "trusted public key $pub added to secrets/worker_trusted_public_keys"
  fi

  # Redis: ACL file (password stored only as a SHA-256 hash) + the app's URL.
  if [[ ! -s secrets/redis_acl || ! -s secrets/redis_url ]]; then
    local rpw
    rpw=$(openssl rand -hex 24)
    write_secret redis_acl "$(printf '%s\n' \
      'user default off' \
      "user payment on #$(printf '%s' "$rpw" | sha256sum | cut -d' ' -f1) ~* &* +@all -@dangerous" \
      'user healthcheck on nopass -@all +ping')"$'\n'
    write_secret redis_url "redis://payment:$rpw@redis:6379/0"
    GENERATED+=(redis_acl redis_url)
    say "generated secrets/redis_acl + secrets/redis_url"
  fi

  # NATS: auth include for nats.conf + the workers' URL.
  if [[ ! -s secrets/nats_auth.conf || ! -s secrets/nats_url ]]; then
    local npw
    npw=$(openssl rand -hex 24)
    write_secret nats_auth.conf "$(printf 'authorization {\n  user: "payment"\n  password: "%s"\n}' "$npw")"$'\n'
    write_secret nats_url "nats://payment:$npw@nats:4222"
    GENERATED+=(nats_auth.conf nats_url)
    say "generated secrets/nats_auth.conf + secrets/nats_url"
  fi

  # HA overlay (ha/docker-compose.ha.yml): replication, Patroni REST API and
  # etcd credentials (generated always; unused without the overlay).
  gen_secret pg_replication_password 24
  gen_secret patroni_api_password    24
  gen_secret etcd_root_password      24
  gen_secret etcd_patroni_password   24

  # Optional off-box copy target for the backup sidecars (rclone config). An
  # empty placeholder keeps compose happy when no target is configured.
  [[ -f secrets/backup_rclone_conf ]] || write_secret backup_rclone_conf ""
}

# ensure_backup_key <backup image>: an age key pair for the backup encryption,
# unless backup_recipients already holds age recipients or an OpenPGP key.
ensure_backup_key() {
  [[ -s secrets/backup_recipients ]] && return 0
  local identity
  if command -v age-keygen >/dev/null 2>&1; then
    identity=$(age-keygen 2>/dev/null)
  else
    identity=$(docker run --rm --network none --entrypoint age-keygen "$1" 2>/dev/null)
  fi
  [[ "$identity" == *AGE-SECRET-KEY-* ]] || die "could not generate the backup encryption key"
  ( umask 077; printf '%s\n' "$identity" > secrets/backup_age_identity )
  printf '%s\n' "$identity" | sed -n 's/^# public key: //p' > secrets/backup_recipients
  GENERATED+=(backup_age_identity backup_recipients)
  say "generated the backup encryption key (age): secrets/backup_recipients (public) + secrets/backup_age_identity (PRIVATE)"
}

# apply_secret_perms <helper image>: dir 0700; files 0640 group SECRETS_GID;
# the backup identity 0600. chgrp needs root (or membership of the group), so
# it falls back to a throwaway root container with only CHOWN/FOWNER.
apply_secret_perms() {
  local img=$1 f need=0
  chmod 700 secrets
  for f in secrets/*; do
    [[ -f "$f" ]] || continue
    if [[ "$f" == secrets/backup_age_identity ]]; then chmod 600 "$f"; continue; fi
    chmod 640 "$f"
    [[ "$(stat -c %g "$f")" == "$SECRETS_GID" ]] || need=1
  done
  [[ $need -eq 1 ]] || return 0
  if find secrets -maxdepth 1 -type f ! -name backup_age_identity -exec chgrp "$SECRETS_GID" {} + 2>/dev/null; then
    return 0
  fi
  docker run --rm --user 0:0 --network none --cap-drop ALL --cap-add CHOWN --cap-add FOWNER \
    --security-opt no-new-privileges -v "$PWD/secrets:/s" --entrypoint /bin/sh "$img" \
    -c 'find /s -maxdepth 1 -type f ! -name backup_age_identity -exec chgrp "$0" {} + && find /s -maxdepth 1 -type f ! -name backup_age_identity -exec chmod 0640 {} +' \
    "$SECRETS_GID" \
    || die "could not set group $SECRETS_GID on deploy/secrets/* (run deploy.sh as root once, or add yourself to group $SECRETS_GID)"
}

# The postgres image the compose file pins (helper containers reuse it).
pg_image_ref() {
  sed -n 's/^[[:space:]]*image:[[:space:]]*\(postgres:[^[:space:]]*\).*/\1/p' docker-compose.prod.yml | head -n1
}

volume_exists() { docker volume inspect "${PROJECT}_$1" >/dev/null 2>&1; }

# The list of keys an operator must hold OFF this box.
print_offbox_keys() {
  cat <<'MSG'

!! Copy these OFF THE BOX now (password manager / escrow) and keep them out of git:
!!   secrets/worker_signing_key          signs the tamper-evidence checkpoints
!!   secrets/worker_trusted_public_keys  what the off-box restore drill verifies against
!!   secrets/biometric_template_key      without it every fingerprint enrolment is unreadable
!!   secrets/backup_age_identity         the ONLY way to decrypt the backups (then delete it
!!                                       here if backups must stay unreadable to a box compromise)
!!   secrets/postgres_password           bootstrap superuser (emergencies)
!!   secrets/pgbackrest.conf             (WAL overlay) holds the repository cipher passphrase
!! Everything else under secrets/ is regenerated on demand and can be rotated.
MSG
}
