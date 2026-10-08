#!/usr/bin/env bash
# Static checks of the HA overlay (CI, and before any change to deploy/ha):
# every documented compose combination with the HA overlay parses, nothing but
# Caddy (and Grafana on loopback) publishes a port, the members/etcd sit only
# on internal networks, every image is pinned by digest, and haproxy.cfg is
# valid for both the compose and the local addresses (with HAPROXY_IMAGE set,
# through that image; else a local haproxy if present).
#
#   scripts/ha-compose-check.sh
#
# Needs docker (the CLI only, no daemon unless HAPROXY_IMAGE is set) and jq.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
cp -a "$ROOT/deploy" "$W/deploy"
cd "$W/deploy"
rm -rf secrets .env app.env
printf '%s\n' 'ADMIN_ALLOW_CIDR=203.0.113.7/32' 'LAN_BIND_ADDRESS=192.168.1.156' \
  'GRAFANA_PASSWORD=ci' 'ALERT_WEBHOOK_URL=http://example/ci' 'ALERT_HEARTBEAT_URL=http://example/hb' > .env
mkdir -p secrets
for s in postgres_password pg_owner_password pg_app_password pg_backup_password pg_monitor_password \
         database_url migration_database_url jwt_secret worker_signing_key worker_trusted_public_keys \
         biometric_template_key redis_acl redis_url nats_auth.conf nats_url backup_recipients \
         backup_rclone_conf pgbackrest.conf; do echo ci > "secrets/$s"; done
HA_SECRETS_DIR=secrets ha/init-secrets.sh > /dev/null
cp app.env.example app.env

fail() { echo "FAIL: $*" >&2; exit 1; }
for combo in docker-compose.prod.yml:ha/docker-compose.ha.yml \
             docker-compose.prod.yml:docker-compose.monitoring.yml:ha/docker-compose.ha.yml:ha/docker-compose.ha-monitoring.yml \
             docker-compose.lan.yml:ha/docker-compose.ha.yml \
             docker-compose.prod.yml:ha/docker-compose.ha.yml:ha/docker-compose.ha-drill.yml; do
  echo "compose config: $combo"
  COMPOSE_FILE=$combo docker compose config -q
done

json=$(COMPOSE_FILE=docker-compose.prod.yml:docker-compose.monitoring.yml:ha/docker-compose.ha.yml:ha/docker-compose.ha-monitoring.yml \
       docker compose config --format json)
jq -e '[.services | to_entries[] | select(.value.ports) | .key] | sort == ["caddy", "grafana"]' <<< "$json" > /dev/null \
  || fail "a service other than caddy/grafana publishes a port"
jq -e '.services.postgres.image | startswith("haproxy:")' <<< "$json" > /dev/null || fail "postgres is not HAProxy"
jq -e '.services.postgres | (has("volumes") or has("secrets")) | not' <<< "$json" > /dev/null \
  || fail "the single-node postgres settings leaked into the HAProxy service"
jq -e '.networks.ha.internal == true' <<< "$json" > /dev/null || fail "the ha network is not internal"
for svc in pg-1 pg-2 pg-3 etcd-1 etcd-2 etcd-3 etcd-auth; do
  jq -e --arg s "$svc" '.services[$s].networks | has("backend") | not' <<< "$json" > /dev/null \
    || fail "$svc is on the backend network (only HAProxy may be)"
done
jq -e '[.services[] | select(.build | not) | .image | select(test("@sha256:") | not)]
       | map(select(startswith("payment-") | not)) == []' <<< "$json" > /dev/null \
  || fail "an image is not pinned by digest: $(jq -c '[.services[] | select(.build | not) | .image | select(test("@sha256:") | not)]' <<< "$json")"

hacheck() { # hacheck <env assignments...>
  if [[ -n "${HAPROXY_IMAGE:-}" ]]; then
    local e=() kv
    for kv in "$@"; do e+=(-e "$kv"); done
    docker run --rm "${e[@]}" -v "$PWD/ha/haproxy.cfg:/usr/local/etc/haproxy/haproxy.cfg:ro" \
      "$HAPROXY_IMAGE" haproxy -c -f /usr/local/etc/haproxy/haproxy.cfg
  elif command -v haproxy > /dev/null; then
    env "$@" haproxy -c -f ha/haproxy.cfg
  else
    echo "haproxy.cfg: skipped (no HAPROXY_IMAGE, no local haproxy)"
  fi
}
hacheck HA_BIND_RW=:5432 HA_BIND_RO=:5433 HA_BIND_ADMIN=:8404 HA_PG1=pg-1:5432 HA_PG2=pg-2:5432 HA_PG3=pg-3:5432 \
  HA_API1_PORT=8008 HA_API2_PORT=8008 HA_API3_PORT=8008
hacheck HA_BIND_RW=127.0.0.1:5450 HA_BIND_RO=127.0.0.1:5454 HA_BIND_ADMIN=127.0.0.1:7450 \
  HA_PG1=127.0.0.1:5451 HA_PG2=127.0.0.1:5452 HA_PG3=127.0.0.1:5453 HA_API1_PORT=8451 HA_API2_PORT=8452 HA_API3_PORT=8453
echo "HA overlay checks passed"
