#!/bin/bash
# payment-patroni entrypoint: render this member's Patroni configuration — the
# shared /etc/patroni/patroni.yml + identity from the environment (HA_NODE_NAME,
# ...) + credentials from /run/secrets — to /run/patroni/patroni.yml (tmpfs,
# 0600), then become Patroni. Patroni starts, promotes, demotes and rewinds
# Postgres; it is the container's main process, so killing the container kills
# both (what the failover drill does).
set -euo pipefail
python3 /usr/local/lib/ha/render-config.py /etc/patroni/patroni.yml /run/patroni/patroni.yml
exec patroni /run/patroni/patroni.yml
