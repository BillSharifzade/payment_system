#!/usr/bin/env bash
# Prints the compose service name of the member that currently leads (pg-1,
# pg-2 or pg-3), for commands that must run on the primary:
#
#   docker compose exec -T "$(ha/ha-leader.sh)" bash /docker-entrypoint-initdb.d/10-roles.sh
#   docker compose exec "$(ha/ha-leader.sh)" psql -U payment -d payment
#
# Run from deploy/ (COMPOSE_FILE in .env includes ha/docker-compose.ha.yml).
# Asks each running member's Patroni (GET /primary answers 200 on the leader
# only); exits 1 when none leads.
set -euo pipefail
for svc in pg-1 pg-2 pg-3; do
  if docker compose exec -T "$svc" python3 -c \
      "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8008/primary', timeout=2)" >/dev/null 2>&1; then
    echo "$svc"
    exit 0
  fi
done
echo "ha-leader.sh: no member is leading (docker compose exec pg-1 patronictl list)" >&2
exit 1
