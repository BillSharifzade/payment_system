#!/usr/bin/env bash
# One-command deploy for a single server. Generates secrets on first run, then
# builds and starts the stack. Idempotent: safe to re-run to roll out updates.
set -euo pipefail
cd "$(dirname "$0")"

COMPOSE="docker compose -f docker-compose.prod.yml"

# 1. Create .env with strong generated secrets on first run.
if [[ ! -f .env ]]; then
  echo "==> No .env found; generating one with fresh secrets..."
  cp .env.prod.example .env
  PG=$(openssl rand -hex 16)
  JWT=$(openssl rand -hex 32)
  SIGN=$(openssl rand -hex 32)
  sed -i "s|^POSTGRES_PASSWORD=.*|POSTGRES_PASSWORD=${PG}|" .env
  sed -i "s|^JWT_SECRET=.*|JWT_SECRET=${JWT}|" .env
  sed -i "s|^WORKER_SIGNING_KEY=.*|WORKER_SIGNING_KEY=${SIGN}|" .env
  chmod 600 .env
  echo "==> Wrote deploy/.env (chmod 600). BACK UP WORKER_SIGNING_KEY somewhere safe."
  echo "==> Set SITE_ADDRESS=your.domain in deploy/.env to enable automatic HTTPS."
fi

# 2. Build the image and bring the stack up.
echo "==> Building and starting..."
$COMPOSE up -d --build

# 3. Wait for the API to answer through Caddy.
echo "==> Waiting for /health ..."
for i in $(seq 1 60); do
  if curl -fsS http://localhost:80/health >/dev/null 2>&1; then
    echo "==> Up. $(curl -s http://localhost:80/health)"
    $COMPOSE ps
    exit 0
  fi
  sleep 2
done
echo "!! /health did not come up in time. Check: $COMPOSE logs --tail=50" >&2
exit 1
