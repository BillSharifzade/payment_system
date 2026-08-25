# Deploying the payment system

A single-server production stack (Docker Compose) that you can stand up in one
command, structured so each piece later lifts into the multi-node HA design
without touching the app.

## What runs

| Service | Image | Role |
|---|---|---|
| `caddy` | caddy:2 | TLS termination + reverse proxy + rate-limit source IP (`X-Forwarded-For`) |
| `payment-server` | built here | the HTTP API (runs DB migrations on startup) |
| `payment-workers` | built here | checkpoint sealer, chain verifier, reconciliation, outbox→NATS relay |
| `postgres` | postgres:16 | the ledger (durable named volume) |
| `redis` | redis:7 | shared rate-limit counters |
| `nats` | nats:2 (JetStream) | event stream (`payments.transaction.posted`) |

Only Caddy publishes ports (80/443). Everything else is on the internal Docker
network. The two app services share **one image**; workers just override the
entrypoint.

## Prerequisites (on the server)

- Docker Engine + Compose plugin (`docker compose version`)
- Ports 80/443 open; a DNS record pointing at the server if you want HTTPS

## Quick start

```bash
git clone <repo> && cd payment_system
./deploy/deploy.sh
```

`deploy.sh` on first run:
1. creates `deploy/.env` from the example with **freshly generated secrets**
   (`POSTGRES_PASSWORD`, `JWT_SECRET`, `WORKER_SIGNING_KEY`), `chmod 600`;
2. builds the image and starts the stack;
3. waits for `/health`.

Then edit `deploy/.env` to set `SITE_ADDRESS=payments.yourdomain.com` and re-run
`./deploy/deploy.sh` — Caddy provisions a Let's Encrypt certificate automatically.

> ⚠️ **Back up `WORKER_SIGNING_KEY`.** It signs the tamper-evidence checkpoints;
> if it changes, previously-sealed checkpoints stop verifying.

## First admin

There is no public "make admin" endpoint (by design). Promote a registered user
directly in the DB:

```bash
docker compose -f deploy/docker-compose.prod.yml exec postgres \
  psql -U payment -d payment -c \
  "UPDATE users SET is_admin=true WHERE phone='<their-phone>';"
```

## Operate

```bash
C="docker compose -f deploy/docker-compose.prod.yml"
$C ps                       # status
$C logs -f payment-server   # follow logs (structured JSON)
$C logs -f payment-workers  # watch sealing + reconciliation
git pull && ./deploy/deploy.sh   # roll out an update (rebuild + recreate)
$C --scale payment-server=2 up -d   # run 2 API replicas (Caddy load-balances)
```

**Watch these two log lines — they are your money-integrity tripwires:**
- `RECONCILIATION FAILED` (from `payment-workers`) — balances no longer sum to
  zero or don't match their entries. Page a human.
- `CHAIN VERIFICATION FAILED` (from `payment-workers`) — a checkpoint failed
  the periodic independent verification (possible tampering). The full chain is
  re-verified from genesis on every worker start, then incrementally every
  `VERIFY_INTERVAL_SECS`.

### Backups

```bash
docker compose -f deploy/docker-compose.prod.yml exec postgres \
  pg_dump -U payment payment | gzip > backup-$(date +%F).sql.gz
```
Schedule this (cron) and copy off-box. The `pgdata` volume is the source of truth.

## Admin console

The ops console (`console/`) is a static SPA that Caddy serves at `/admin`
(same origin as the API, so no CORS). Build it before `deploy.sh`:

```bash
cd console && npm install && npm run build   # produces console/dist
```

Sign in with an admin account (promote one via SQL as above — the console
refuses non-admin logins). Consider IP-allowlisting the `/admin` block in the
Caddyfile once your ops network is known.

## Monitoring (optional)

```bash
docker compose -f deploy/docker-compose.prod.yml \
               -f deploy/docker-compose.monitoring.yml up -d
```
Ships JSON logs to Loki; Grafana is bound to `127.0.0.1:3000` (reach it via
`ssh -L 3000:localhost:3000 user@server`). Set `GRAFANA_PASSWORD` in `.env`.

## Security checklist

- [x] App refuses to boot on dev defaults (`APP_ENV=prod` requires a real `JWT_SECRET`).
- [x] Secrets in `.env` (chmod 600), not baked into the image. *(Upgrade path: Vault.)*
- [x] Only Caddy is internet-facing; DB/Redis/NATS are internal-only.
- [x] Distroless + nonroot runtime (no shell in the app image).
- [ ] Put the server behind a firewall; allow only 80/443 (+ SSH).
- [ ] Use a domain so Caddy enables HTTPS — do not serve real traffic on `:80`.

## The upgrade path to HA (when one box isn't enough)

This layout maps 1:1 onto the DESIGN.md §12 target:
1. **Postgres HA first** (the load-bearing move): move Postgres off Compose to a
   3-node **Patroni + etcd** cluster (bare metal). Point `DATABASE_URL` at the
   Patroni VIP. Automatic failover, no data loss. *Nothing in the app changes.*
2. **App tier to k3s/k8s**: the same image + env become a Deployment (API, N
   replicas) + a single-replica worker Deployment. Caddy → an Ingress.
3. **Secrets to Vault**, **logs/metrics** to the full Prometheus+Grafana+Loki
   stack, **GitOps** via Argo CD.
4. **Scale writes** (only if needed): swap `PostgresLedger` for the
   `TigerBeetleLedger` behind the existing `LedgerStore` trait.
