# Deploying the payment system

A single-server production stack (Docker Compose) that stands up in one command
and is structured so each piece later lifts into the multi-node HA design
without touching the app.

## What runs

| Service | Image | Role |
|---|---|---|
| `caddy` | `payment-console` (built: Caddy 2 + the admin SPA) | TLS termination, reverse proxy (`/v1`, `/health`, `/ready`), `/admin` console, access log |
| `payment-server` | `payment-system` (built) | the HTTP API; runs migrations on startup; metrics on `:9100` (internal) |
| `payment-workers` | `payment-system` (same image, other entrypoint) | checkpoint sealer, chain verifier, reconciliation, outbox → NATS JetStream relay; metrics on `:9101` |
| `postgres` | postgres 16 | the ledger (named volume, page checksums, tuned for a 4 GB box) |
| `redis` | redis 7 | shared rate-limit counters (bounded memory, LRU) |
| `nats` | nats 2 (JetStream) | durable event stream `payments.transaction.posted` |
| `postgres-backup` | postgres-backup-local | daily logical dumps into the `backups` volume (14 d / 8 w / 12 m) |
| `kyc-backup` | alpine cron | nightly tarball of the KYC document volume into `backups` |

Only Caddy publishes ports (80/443). Every image tag is pinned; every service
runs with `no-new-privileges`, dropped capabilities, a read-only root
filesystem where possible, a memory limit, log rotation (50 MB × 5), a
healthcheck and a graceful stop period. The app containers start only after
Postgres/Redis/NATS are *healthy*, the workers only after the server has
migrated, Caddy only after the server is ready.

## Prerequisites (on the server)

- Docker Engine + Compose plugin v2.23 or newer (`docker compose version`)
- Ports 80/443 open; a DNS record pointing at the server for public HTTPS
- ~4 GB RAM; the images are built here (cargo-chef caches the dependency
  layer, so a source-only rebuild takes a minute or two after the first build).
  A 2-vCPU box should instead receive images built elsewhere — see "LAN box".
- Nothing else: no Rust, no Node (the console is built inside its image).

## Quick start

```bash
git clone <repo> && cd payment_system/deploy
./deploy.sh
```

`deploy.sh` on first run:

1. creates `deploy/.env` (compose-level settings) and `deploy/app.env` (the
   application's settings) from their `.example` files;
2. generates the secrets as **files** under `deploy/secrets/` (directory
   `0700`, files `0644` — Compose bind-mounts them with host permissions and
   the app runs as a non-root user, so the directory is the boundary):
   `postgres_password`, `database_url`, `jwt_secret`, `worker_signing_key`.
   The app reads them through the `*_FILE` variables; nothing secret is ever in
   `.env`, in the image, or in `docker inspect` output;
3. builds `payment-system:<git-sha>` and `payment-console:<git-sha>` (also
   tagged `latest`), starts the stack and waits for
   `payment-server healthcheck` (the binary probing its own `/ready`).

Then set `SITE_ADDRESS=payments.yourdomain.com` in `deploy/.env` and re-run
`./deploy.sh` — Caddy provisions a Let's Encrypt certificate and enables HSTS.

> **Back up `deploy/secrets/worker_signing_key`** (it signs the tamper-evidence
> checkpoints; a lost key means old checkpoints can never be re-verified) and
> `postgres_password`. Escrow them with the pgBackRest passphrase if you enable
> WAL archiving.

### Rollback

Every deploy tags the images with the git SHA and keeps the previous ones:

```bash
docker images 'payment-*'
IMAGE_TAG=<previous-sha> ./deploy.sh --no-build
```

Migrations are forward-only: roll back the app only to a build whose schema
version the database already has (they are additive, so the previous release
runs fine on the newer schema).

## Settings

- `deploy/.env` — compose-level: `COMPOSE_FILE` (base file + overlays),
  `SITE_ADDRESS`, `ADMIN_ALLOW_CIDR`, `TZ`, and for the monitoring overlay
  `GRAFANA_PASSWORD` and `ALERT_WEBHOOK_URL`. Not passed to any container.
- `deploy/app.env` — everything the two binaries read (timeouts, pool size,
  rate limits, fees, AML tiers, worker cadences, retention). **Every numeric
  value is parsed strictly**: a typo refuses to boot rather than silently
  reverting a compliance limit. `TRANSFER_FEE_BPS` must be ≤ 10000.
- `deploy/secrets/*` — see above; `DATABASE_URL_FILE`, `JWT_SECRET_FILE`,
  `WORKER_SIGNING_KEY_FILE` (and optional `REDIS_URL_FILE`, `NATS_URL_FILE`).

## First admin

There is no public "make admin" endpoint (by design). Promote a registered user
directly in the database (run it on the server; phone is digits only):

```bash
docker compose exec postgres psql -U payment -d payment \
  -c "UPDATE users SET is_admin=true WHERE phone='992XXXXXXXXX';"
```

## Operate

All commands from `deploy/` (Compose picks up `.env` → `COMPOSE_FILE`):

```bash
docker compose ps                          # status + health of every service
docker compose logs -f payment-server      # JSON logs incl. the access log (INFO)
docker compose logs -f payment-workers     # sealing, verification, reconciliation
git pull && ./deploy.sh                    # roll out an update
docker compose up -d --scale payment-server=2   # 2 API replicas (Caddy balances on /ready)
docker compose exec caddy tail -f /data/access.log   # Caddy's JSON access log
```

Probes and observability:

- `GET /health` — liveness (process up). `GET /ready` — readiness (database
  answers within 1 s; 503 otherwise). Caddy health-checks `/ready`.
- `payment-server healthcheck` / `payment-workers healthcheck` — the container
  healthchecks (the images have no shell or curl). The workers' check reads a
  heartbeat file the loop touches every tick.
- Every response carries `X-Request-Id`; every error body carries
  `error.request_id`. Ask a user for the "Ref:" shown in the app/console and
  `grep` it in the logs.
- Prometheus metrics on the internal network: `payment-server:9100`,
  `payment-workers:9101` (`http_requests_total{route,method,status}`,
  `http_request_duration_seconds`, `db_pool_size/idle`, `ledger_posts_total`,
  `auth_refresh_total`, `outbox_unsent`, `outbox_oldest_unsent_age_seconds`,
  `ledger_unsealed_transactions`, `ledger_oldest_unsealed_age_seconds`,
  `reconciliation_healthy`, `ledger_chain_verified`, `worker_errors_total`).

**Money-integrity tripwires** (both alert automatically with the monitoring
overlay; without it, watch the worker logs):

- `RECONCILIATION FAILED` — balances no longer sum to zero per currency, or an
  account's balance disagrees with its entries. Page a human; do not restart
  anything until the ledger has been inspected.
- `CHAIN VERIFICATION FAILED` — a signed checkpoint fails re-verification
  (possible tampering). Preserve the database and logs first.

## Backups and the restore drill

Two sidecars write into the `backups` named volume:

- `postgres-backup`: a logical dump daily and on every (re)start (so one is
  taken right before each deploy), rotated 14 daily / 8 weekly / 12 monthly.
- `kyc-backup`: a nightly tarball of the uploaded identity documents (they are
  files, not rows — `pg_dump` alone would lose them), 14-day retention.

Copy the volume off-box (encrypted) on a schedule, for example with restic or
`rclone crypt`; a backup on the same disk as the database is not a backup:

```bash
docker run --rm -v payment_backups:/backups:ro -v ~/.restic:/root/.restic restic/restic \
  -r s3:s3.example.com/payment-backups backup /backups
```

To encrypt individual dumps before shipping them elsewhere:
`gzip -dc dump.sql.gz | gpg --encrypt -r ops@example.com > dump.sql.gpg`
(the restore drill accepts `.sql.gz.gpg` files).

**Restore drill** — run monthly from cron, and after any change to backups:

```bash
./restore-drill.sh                # newest dump in the backups volume
./restore-drill.sh /path/dump.sql.gz   # a copy fetched back from off-box storage
```

It restores into a throwaway Postgres on tmpfs (the live stack is untouched),
then runs the same checks the workers run continuously: every currency sums to
zero and every balance equals the signed sum of its entries. Exit 0 = PASS.

### WAL archiving (point-in-time recovery)

Nightly dumps bound your loss to a day. For minutes, enable the opt-in overlay
`docker-compose.wal-archive.yml`: Postgres is rebuilt with pgBackRest
(`Dockerfile.postgres`), WAL is shipped continuously to an off-box
S3-compatible bucket, encrypted client-side. Steps are at the top of that file;
the configuration template is `pgbackrest/pgbackrest.conf.example`.

## Admin console

Caddy serves the console at `https://<SITE_ADDRESS>/admin/` from the
`payment-console` image (same origin as the API, so no CORS). Sign in with an
admin account. Restrict who can reach it with `ADMIN_ALLOW_CIDR` in `.env`
(space-separated IPs/CIDRs; unset = everyone). The `/admin` response policy is a
strict CSP without inline scripts or styles, cached assets for a year (they are
content-hashed) and an always-revalidated shell.

## Monitoring (optional overlay)

```bash
# in deploy/.env
COMPOSE_FILE=docker-compose.prod.yml:docker-compose.monitoring.yml
GRAFANA_PASSWORD=...          # required
ALERT_WEBHOOK_URL=https://... # required: ntfy / Slack bridge / PagerDuty ...
./deploy.sh
```

Adds promtail → Loki (30-day retention, compactor), Prometheus (30 d / 2 GB)
scraping both binaries, Alertmanager posting to your webhook, and Grafana on
`127.0.0.1:3000` (reach it via `ssh -L 3000:localhost:3000 user@server`;
Prometheus, Loki and Alertmanager are pre-provisioned as datasources).

Alerts out of the box: reconciliation failed, chain verification failed, no
checkpoint sealed for 15 min, container restarts, 5xx rate > 1 %, pool
exhausted, request timeouts, outbox lag > 60 s, unsealed transactions older
than 5 min, worker errors, scrape target down.

## LAN box (no public 80/443, no domain)

`docker-compose.lan.yml` keeps the same hardened stack but publishes the API on
`:8099` and the console on `:8088` (another application owns 80/443 there). The
box is too small to compile Rust, so build on the dev machine and ship images:

```bash
# dev box
cd deploy && ./deploy.sh            # or just: docker compose build
docker save payment-system:<sha> payment-console:<sha> | ssh box 'docker load'
# box: ~/payment-deploy/ holds docker-compose.lan.yml, .env (COMPOSE_FILE=docker-compose.lan.yml,
#      SITE_ADDRESS=:80), app.env (TRUST_PROXY=false), secrets/, backup/kyc-backup.sh
IMAGE_TAG=<sha> ./deploy.sh --no-build
```

### Internal CA (HTTPS on a bare IP)

With `SITE_ADDRESS=192.168.1.156` Caddy cannot obtain a public certificate, so
it issues one from its own internal CA automatically (the same as writing
`tls internal`). Export the root and trust it on the ops machines and the
staging phones:

```bash
docker compose exec caddy cat /data/caddy/pki/authorities/local/root.crt > payment-lan-root.crt
```

The mobile `staging` flavour can then pin that root in its
network-security-config and drop the cleartext exception.

## Security checklist

- [x] Binaries refuse to boot on dev defaults outside `APP_ENV=dev`; config typos refuse to boot.
- [x] Secrets are files (`chmod 600`), never in `.env`, images or `docker inspect`. *(Upgrade path: Vault.)*
- [x] Only Caddy is internet-facing; DB/Redis/NATS/metrics are internal-only.
- [x] Distroless + nonroot app image; read-only root filesystems; dropped capabilities; `no-new-privileges`.
- [x] Pinned image tags and CI actions; `cargo deny` + `npm audit` in CI.
- [x] Every admin action is written to `admin_actions` (who funded which wallet, who changed which rate).
- [ ] Put the server behind a firewall; allow only 80/443 (+ SSH).
- [ ] Use a domain so Caddy enables HTTPS — never serve real traffic on `:80`.
- [ ] Copy the `backups` volume off-box and run `restore-drill.sh` monthly.
- [ ] Enable WAL archiving before there is real money in the ledger.

## The upgrade path to HA (when one box isn't enough)

This layout maps 1:1 onto the DESIGN.md §12 target:

1. **Postgres HA first**: a 3-node Patroni + etcd cluster; point
   `secrets/database_url` at its VIP. *Nothing in the app changes.*
2. **App tier to k3s/k8s**: the same image + `app.env` become a Deployment (API,
   N replicas) + a single-replica worker Deployment; Caddy → an Ingress. The
   `healthcheck` subcommands become the liveness/readiness probes.
3. **Secrets to Vault**, GitOps via Argo CD.
4. **Scale writes** only if needed: swap `PostgresLedger` for
   `TigerBeetleLedger` behind the existing `LedgerStore` trait. One box does
   ~3 000 fee-bearing transfers/s today.
