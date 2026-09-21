#!/usr/bin/env bash
# One-command deploy for a single server. Idempotent: re-run to roll out.
#
#   ./deploy.sh              build both images (tag = git SHA, also `latest`), roll out
#   ./deploy.sh --no-build   roll out images that are already present (the LAN
#                            box: images are built on the dev box and shipped
#                            with `docker save | ssh ... docker load`)
#   IMAGE_TAG=<old-sha> ./deploy.sh --no-build      roll BACK to a previous image
#
# First run creates deploy/.env (compose-level settings), deploy/app.env (the
# app's settings) and deploy/secrets/* (generated, chmod 600, never in .env).
set -euo pipefail
cd "$(dirname "$0")"

BUILD=1
for arg in "$@"; do
  case "$arg" in
    --no-build) BUILD=0 ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

# ---- 1. Settings files ------------------------------------------------------
first_run=0
if [[ ! -f .env ]]; then
  cp .env.prod.example .env && chmod 600 .env
  first_run=1
  echo "==> created deploy/.env — set SITE_ADDRESS (and COMPOSE_FILE for the LAN box)"
fi
if [[ ! -f app.env ]]; then
  cp app.env.example app.env && chmod 600 app.env
  first_run=1
  echo "==> created deploy/app.env (application settings)"
fi
# Compose reads deploy/.env for interpolation; COMPOSE_FILE inside it selects
# the base file and overlays. Honour an explicit COMPOSE_FILE from the caller.
if [[ -z "${COMPOSE_FILE:-}" ]]; then
  COMPOSE_FILE=$(sed -n 's/^COMPOSE_FILE=//p' .env | tail -n1)
fi
export COMPOSE_FILE="${COMPOSE_FILE:-docker-compose.prod.yml}"

# ---- 2. Secrets as files (generated once) -----------------------------------
mkdir -p secrets && chmod 700 secrets
gen_secret() { # gen_secret <name> <command...>
  local name=$1; shift
  if [[ ! -s "secrets/$name" ]]; then
    (umask 022; "$@" > "secrets/$name.tmp" && mv "secrets/$name.tmp" "secrets/$name")
    echo "==> generated secrets/$name"
  fi
}
gen_secret postgres_password    openssl rand -hex 24
gen_secret jwt_secret           openssl rand -hex 32
gen_secret worker_signing_key   openssl rand -hex 32
if [[ ! -s secrets/database_url ]]; then
  (umask 022; printf 'postgres://payment:%s@postgres:5432/payment' "$(cat secrets/postgres_password)" > secrets/database_url)
  echo "==> generated secrets/database_url"
fi
# The directory is 0700 (only the deploy user can reach it). The FILES are
# 0644 on purpose: Compose bind-mounts each secret into the containers that
# declare it, keeping host ownership, and ignores `mode:` for file secrets — a
# 0600 file owned by this user is unreadable by the app (nonroot uid 65532) and
# by any root that has CAP_DAC_OVERRIDE dropped. Each container sees only the
# secrets it is granted, so the host directory permission is the real boundary.
chmod 644 secrets/* 2>/dev/null || true
if [[ $first_run -eq 1 ]]; then
  cat <<'MSG'

!! BACK UP deploy/secrets/worker_signing_key NOW (it signs the tamper-evidence
!! checkpoints; a lost key means old checkpoints can never be re-verified) and
!! deploy/secrets/postgres_password. Keep them out of the repository.
MSG
fi

# ---- 3. Image tag -------------------------------------------------------------
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
echo "==> IMAGE_TAG=$IMAGE_TAG  COMPOSE_FILE=$COMPOSE_FILE"

# ---- 4. Build ------------------------------------------------------------------
if [[ $BUILD -eq 1 ]]; then
  echo "==> building images (cargo-chef caches the dependency layer)..."
  docker compose build
  docker tag "payment-system:$IMAGE_TAG"  payment-system:latest
  docker tag "payment-console:$IMAGE_TAG" payment-console:latest
else
  for img in payment-system payment-console; do
    docker image inspect "$img:$IMAGE_TAG" >/dev/null 2>&1 \
      || { echo "!! image $img:$IMAGE_TAG not present (docker load it, or drop --no-build)" >&2; exit 1; }
  done
fi

# ---- 5. Roll out -----------------------------------------------------------------
echo "==> starting..."
docker compose up -d --remove-orphans

# ---- 6. Wait for readiness --------------------------------------------------------
# The binary probes its own /ready (distroless image: no curl). Falls back to
# curl through Caddy, which only answers once its upstream health check passes.
echo "==> waiting for payment-server to be ready..."
ready=0
for _ in $(seq 1 60); do
  if docker compose exec -T payment-server /usr/local/bin/payment-server healthcheck >/dev/null 2>&1; then
    ready=1; break
  fi
  site=$(sed -n 's/^SITE_ADDRESS=//p' .env | tail -n1)
  if [[ -n "$site" && "$site" != :* ]] \
     && curl -fsSk --max-time 3 -H "Host: $site" "https://127.0.0.1/ready" >/dev/null 2>&1; then
    ready=1; break
  fi
  sleep 2
done
if [[ $ready -ne 1 ]]; then
  echo "!! payment-server did not become ready in 120 s. Inspect: docker compose ps; docker compose logs --tail=100 payment-server" >&2
  exit 1
fi
docker compose ps
cat <<MSG

==> Deployed $IMAGE_TAG.
    Roll back:   IMAGE_TAG=<previous-sha> ./deploy.sh --no-build
    Images kept: docker images 'payment-*'
    Logs:        docker compose logs -f payment-server payment-workers
MSG
