# Deploying the payment system

A single-server production stack (Docker Compose) that stands up with one
script and is structured so each piece later lifts into the multi-node HA
design without touching the app.

## What runs

| Service | Image | Role |
|---|---|---|
| `caddy` | `payment-console` (built: Caddy 2 + the admin SPA, non-root) | TLS termination, reverse proxy (`/v1`, `/health`, `/ready`) with one health-checked upstream per API replica, `/admin` console, **admin allowlist**, access log |
| `payment-server` | `payment-system` (built) | the HTTP API; runs migrations at startup **as `payment_owner`**, serves **as `payment_app`**; metrics on `:9100` (internal) |
| `payment-workers` | `payment-system` (same image, other entrypoint) | checkpoint sealer, chain verifier, reconciliation, outbox → NATS relay (leader-elected); metrics on `:9101` |
| `postgres` | postgres 16 (pinned digest), runs as uid 999 | the ledger (named volume, page checksums, tuned for a 4 GB box) |
| `redis` | redis 7 (ACL auth) | shared rate-limit counters (bounded memory, LRU) |
| `nats` | nats 2 (JetStream, user/password auth) | durable event stream `payments.transaction.posted` |
| `postgres-backup` | `payment-backup` (built: pg_dump 16 + age + gnupg + rclone, non-root) | daily **encrypted** dump as the read-only `payment_backup` role + one before every deploy; optional off-box copy |
| `kyc-backup` | `payment-backup` | nightly **encrypted** tarball of the KYC document volume |

Every image is pinned by digest; every service runs with `no-new-privileges`,
**all** capabilities dropped, a non-root user where the image allows (all but
NATS), a read-only root filesystem where possible, memory/pid limits, log
rotation (50 MB × 5), a healthcheck and a graceful stop period. The app
containers start only after Postgres/Redis/NATS are *healthy*, the workers
only after the server has migrated, Caddy only after the server is ready.

### Networks — only Caddy publishes ports

| Network | Members | Egress |
|---|---|---|
| `edge` | caddy, payment-server | yes (ACME) |
| `backend` | payment-server, payment-workers, postgres, redis, nats, postgres-backup (+ postgres-exporter) | **none** (`internal`) |
| `egress` | the backup sidecars (off-box copy), alertmanager, grafana, postgres in the WAL overlay | yes |
| `monitoring` (overlay) | prometheus, loki, promtail, grafana, alertmanager, exporters, the two app binaries | **none** (`internal`) |

Grafana's port is bound to `127.0.0.1` only. Nothing else publishes a port.

### Database roles

| Role | Used by | Can |
|---|---|---|
| `payment` | init + emergencies (`docker compose exec postgres psql -U payment`) | everything (bootstrap superuser) |
| `payment_owner` | `MIGRATION_DATABASE_URL` — migrations at server startup only | owns the database and schema `public` |
| `payment_app` | `DATABASE_URL` of payment-server and payment-workers | DML only; **no** UPDATE/DELETE on `entries`, `checkpoints`, `admin_actions`, `screening_events`, `biometric_events`, `voided_transactions`; on `transactions` only INSERT + UPDATE of `sealed_seq`; read-only `_sqlx_migrations`; no DDL, cannot disable triggers |
| `payment_backup` | the dump sidecar | `pg_read_all_data` (read-only) |
| `payment_monitor` | postgres-exporter | `pg_monitor` + SELECT on `entries`/`accounts` (deposit alert) |

`deploy/postgres/10-roles.sh` creates the roles from secret files (on the
first start of a fresh volume, and again on every deploy so a rotated password
file takes effect). Migration `0027_db_role_grants.sql` grants the table
privileges and sets default privileges so tables created by later migrations
get the same DML baseline — **a new append-only table must REVOKE UPDATE,
DELETE from `payment_app` in the migration that creates it.** On dev/test
clusters without these roles, 0027 is a no-op. `scripts/test-db-grants.sh`
proves the whole model on a scratch database (CI job `db-roles`).

## Prerequisites (on the server)

- Docker Engine ≥ 20.10 + Compose plugin v2.23 or newer (`docker compose version`)
- Ports 80/443 open; a DNS record pointing at the server for public HTTPS
- ~4 GB RAM; the images are built here (cargo-chef caches the dependency
  layer, so a source-only rebuild takes a minute or two after the first build).
  A 2-vCPU box should instead receive images built elsewhere — see "LAN box".
- Nothing else: no Rust, no Node, no age/gpg (everything runs in images).

## Quick start (fresh install)

```bash
git clone <repo> && cd payment_system/deploy
./deploy.sh          # creates .env + app.env, then REFUSES: ADMIN_ALLOW_CIDR is required
$EDITOR .env         # SITE_ADDRESS=payments.example.com, ADMIN_ALLOW_CIDR=<office/VPN IPs>
./deploy.sh
```

`deploy.sh`:

1. creates `deploy/.env` (compose-level settings) and `deploy/app.env` (the
   application's settings) from their `.example` files (0600);
2. **refuses** to deploy while anything is unsafe: no `ADMIN_ALLOW_CIDR`
   (`0.0.0.0/0 ::/0` is accepted as an explicit opt-out with a loud warning), a
   secret or URL set directly in `app.env`, `BIOMETRIC_MATCHER=exact` (or
   unset) or `DEPOSIT_DUAL_CONTROL=false` outside `APP_ENV=dev`, unencrypted
   backups in production, missing overlay settings, or an install from before
   the role split (→ `./upgrade-hardening.sh`);
3. generates the secrets as files under `deploy/secrets/` (see below) and an
   **age key pair for the backups**;
4. builds `payment-system`, `payment-console` and `payment-backup` tagged with
   the git SHA (and `latest`);
5. starts Postgres/Redis/NATS, (re)applies the database roles, and — when the
   database already holds a schema — takes an **encrypted pre-deploy dump of
   the current database before any new server can migrate it** (a failure
   aborts the rollout; `--skip-predeploy-backup` is the emergency override);
6. starts everything and waits until every API replica, the workers and Caddy
   are healthy, then reports the backup sidecars' state.

With `SITE_ADDRESS=payments.yourdomain.com` Caddy provisions a Let's Encrypt
certificate and enables HSTS.

### Secrets (`deploy/secrets/`, never in git, `.env` or images)

The directory is 0700. Files are **0640, group `SECRETS_GID`** (default 10500,
set in `.env`): Compose bind-mounts file secrets with their host owner and
mode, and every container that is handed a secret runs with that supplementary
group — so a container can read exactly the files mounted into it, and no
other host user can even list the directory. (A plain 0600 file would be
unreadable by the containers' non-root users.) `deploy.sh` applies this on
every run (through a throwaway root container when the deploy user may not
`chgrp`).

| File | Consumer | Notes |
|---|---|---|
| `postgres_password` | postgres | bootstrap superuser — **back up off-box** |
| `pg_owner_password`, `pg_app_password`, `pg_backup_password`, `pg_monitor_password` | postgres (10-roles.sh) + the services below | rotate: replace the file, `./deploy.sh` |
| `database_url` (payment_app), `migration_database_url` (payment_owner) | server / workers | derived from the password files on every deploy |
| `jwt_secret` | server | rotating it logs everyone out |
| `worker_signing_key` | workers | signs the checkpoints — **back up off-box** |
| `worker_trusted_public_keys` | workers, `restore-drill.sh` | comma-separated hex Ed25519 keys the verifier accepts; the current key's is added automatically. **Keep old keys here when rotating the signing key**, and keep a copy off-box for off-box drills |
| `biometric_template_key` | server | seals fingerprint templates — **back up off-box**; losing it makes every enrolment unreadable |
| `redis_acl` (password as SHA-256 only) + `redis_url` | redis / server | user `payment`, dangerous commands removed; `default` user disabled |
| `nats_auth.conf` + `nats_url` | nats / workers | `authorization { user, password }` included by `nats/nats.conf` |
| `backup_recipients` | backup sidecars | age recipient(s) or an ASCII-armoured OpenPGP public key |
| `backup_age_identity` (0600, never mounted) | you, `restore-drill.sh` | the ONLY key that decrypts the backups — **copy it off-box**, then delete it from the server if backups must stay unreadable to a compromised box (pass `-i` to the drill) |
| `backup_rclone_conf` | backup sidecars | optional off-box target (empty by default) |
| `pgbackrest.conf` | postgres (WAL overlay) | holds the repository cipher passphrase — **escrow it** |

`deploy.sh` prints this off-box list whenever it generates something new.

### Upgrading an install from before this layout

Installs deployed before the 2026-10 hardening ran the app as the database
superuser, with unauthenticated Redis/NATS and plaintext backups. `deploy.sh`
detects them and stops. Convert once, then deploy:

```bash
cd payment_system && git pull && cd deploy
$EDITOR .env                    # add ADMIN_ALLOW_CIDR=... (required now)
./upgrade-hardening.sh          # idempotent; keeps app.env.bak-*, .env.bak-*, secrets/.bak-*/
./deploy.sh                     # encrypted pre-deploy dump, rollout, migration 0027
```

`upgrade-hardening.sh`:

1. moves a real `BIOMETRIC_TEMPLATE_KEY` from `app.env` into
   `secrets/biometric_template_key` (enrolments stay readable) and removes
   every plain secret/URL variable from `app.env` (`REDIS_URL`, `NATS_URL`,
   `DATABASE_URL`, … — compose now provides the `*_FILE` variants; on the LAN
   overlay it also sets `TRUST_PROXY=true`, the API now sits behind Caddy);
2. generates the new secrets and rewrites `database_url` — which held the
   **superuser's** credentials — to the `payment_app` role;
3. creates the four roles and transfers every object in schema `public` from
   the superuser to `payment_owner` (`postgres/roles.psql` +
   `postgres/reassign-ownership.psql`, through the running postgres container
   or a temporary one on the data volume). The old app keeps working meanwhile;
4. chowns the `backups` volume to uid 65532 (the backup sidecars no longer run
   as root) and `caddydata`/`caddyconfig` to uid 10001 (non-root Caddy).

Then review `app.env` against `app.env.example` (new settings below;
`BIOMETRIC_MATCHER=exact` must become `http` + `BIOMETRIC_MATCHER_URL`). After
`./deploy.sh` the server applies migration 0027 as `payment_owner`; if the
ownership transfer was skipped, 0027 stops the server with a hint instead of
booting an app that cannot read its tables. Old unencrypted dumps in the
volume (`/backups/{last,daily,weekly,monthly}`) are deleted after
`BACKUP_LEGACY_KEEP_DAYS` (30) by the new sidecar; delete the
`*.bak-*` copies once everything works (`secrets/.bak-*/database_url` still
holds the superuser URL).

### Rollback

Every deploy tags the images with the git SHA and keeps the previous ones:

```bash
docker images 'payment-*'
IMAGE_TAG=<previous-sha> ./deploy.sh --no-build
```

Migrations are forward-only: roll back the app only to a build whose schema
version the database already has. The pre-deploy dump in
`/backups/db/pre-deploy/` is the restore point taken just before the rollout.

## Settings

- `deploy/.env` — compose-level: `COMPOSE_FILE` (base file + overlays),
  `SITE_ADDRESS`, `ADMIN_ALLOW_CIDR` (required), `API_REPLICAS`, `SECRETS_GID`,
  `BACKUP_*`, `LAN_BIND_ADDRESS` (LAN overlay), and for the monitoring overlay
  `GRAFANA_PASSWORD`, `ALERT_WEBHOOK_URL`, `ALERT_HEARTBEAT_URL`. Not passed to
  any container. See `.env.prod.example`.
- `deploy/app.env` — everything the two binaries read (timeouts, pool size,
  rate limits, fees, AML tiers, deposits — `DEPOSIT_DUAL_CONTROL`,
  `DEPOSIT_MAX_MINOR` —, biometrics — `BIOMETRIC_MATCHER(_URL)`,
  `BIOMETRIC_IDENTIFY`, `BIOMETRIC_IDENTIFY_SCALE`, `BIOMETRIC_MAX_ATTEMPTS` —,
  device binding — `DEVICE_BINDING`, `DEVICE_MAX_ACTIVE` —, worker cadences
  incl. `VERIFY_FULL_EVERY_SECS`, retention). **Every numeric value is parsed
  strictly**: a typo refuses to boot rather than silently reverting a
  compliance limit. **No secrets or URLs here**: a plain `DATABASE_URL`,
  `REDIS_URL`, … would override the secret file, so `deploy.sh` refuses it.
- `deploy/secrets/*` — see above; the compose files set `DATABASE_URL_FILE`,
  `MIGRATION_DATABASE_URL_FILE`, `JWT_SECRET_FILE`, `REDIS_URL_FILE`,
  `BIOMETRIC_TEMPLATE_KEY_FILE` (server) and `WORKER_SIGNING_KEY_FILE`,
  `WORKER_TRUSTED_PUBLIC_KEYS_FILE`, `NATS_URL_FILE` (workers).

**Fingerprint matcher.** The stack ships no matching engine. Production must
set `BIOMETRIC_MATCHER=http` and `BIOMETRIC_MATCHER_URL` (the defaults in
`app.env.example` point at a `biometric-matcher` service on the `backend`
network); until such a service runs, the API boots and works and only
fingerprint enrolment/payment fail. `exact` is refused outside `APP_ENV=dev`.

## First admin

There is no public "make admin" endpoint (by design). Promote a registered user
directly in the database (run it on the server; phone is digits only):

```bash
docker compose exec postgres psql -U payment -d payment \
  -c "UPDATE users SET is_admin=true WHERE phone='992XXXXXXXXX';"
```

Deposits need **two** admins (dual control): one requests, a different one —
who does not own the wallet — approves in the console.

## Operate

All commands from `deploy/` (Compose picks up `.env` → `COMPOSE_FILE`):

```bash
docker compose ps                          # status + health of every service
docker compose logs -f payment-server      # JSON logs incl. the access log (INFO)
docker compose logs -f payment-workers     # sealing, verification, reconciliation
git pull && ./deploy.sh                    # roll out an update (pre-deploy dump first)
# 2 API replicas: API_REPLICAS=2 in .env, then ./deploy.sh (Caddy gets one
# health-checked upstream per replica; `--scale` alone would not add upstreams)
docker compose exec caddy tail -f /data/access.log   # Caddy's JSON access log
```

Probes and observability:

- `GET /health` — liveness (process up). `GET /ready` — readiness (database
  answers within 1 s; 503 otherwise). Caddy probes `/ready` on **each replica**
  every 5 s; two failures take that replica out of rotation (two successes
  bring it back), while the others keep serving.
- `payment-server healthcheck` / `payment-workers healthcheck` — the container
  healthchecks (the images have no shell or curl). The workers' check reads a
  heartbeat file the loop touches every tick.
- `payment-workers verify-chain` — full re-verification of the checkpoint chain
  against `WORKER_TRUSTED_PUBLIC_KEYS`; one JSON object on stdout, exit 0
  intact / 1 broken / 2 error (used by the restore drill).
- Every response carries `X-Request-Id`; every error body carries
  `error.request_id`.
- Prometheus metrics on the internal network: `payment-server:9100`,
  `payment-workers:9101` (`http_requests_total{route,method,status}`,
  `http_request_duration_seconds`, `db_pool_size/idle`, `ledger_posts_total`,
  `deposits_total`, `auth_refresh_total{outcome}`, `outbox_unsent`,
  `outbox_oldest_unsent_age_seconds`, `ledger_unsealed_transactions`,
  `ledger_oldest_unsealed_age_seconds`, `worker_leader`,
  `reconciliation_healthy`, `reconciliation_full_timestamp_seconds`,
  `ledger_chain_verified`, `ledger_chain_full_verified_timestamp_seconds`,
  `worker_errors_total`).

Workers may run as several replicas: they elect a leader through a Postgres
advisory lock, so their `DATABASE_URL` must be a direct or session-mode
connection — never a transaction-mode pooler.

**Money-integrity tripwires** (alerted with the monitoring overlay; without it,
watch the worker logs):

- `RECONCILIATION FAILED` — balances no longer sum to zero per currency, or an
  account's balance disagrees with its entries. Page a human; do not restart
  anything until the ledger has been inspected.
- `CHAIN VERIFICATION FAILED` — a signed checkpoint fails re-verification
  (possible tampering). Preserve the database and logs first.

## Backups and the restore drill

Two sidecars (image `payment-backup`, uid 65532, no root, no cron) write
**encrypted** files into the `backups` volume:

- `postgres-backup`: a plain-SQL `pg_dump` (gzip, then encrypted) daily at
  `BACKUP_DB_AT` (UTC), as the read-only `payment_backup` role, plus one before
  every deploy (`db/pre-deploy/`). Rotation 14 daily / 8 weekly / 12 monthly,
  pre-deploy dumps 30 days. Layout: `/backups/db/{daily,weekly,monthly,pre-deploy}/payment-<UTC>.sql.gz.age` (+ `.sha256`).
- `kyc-backup`: a tarball of the uploaded identity documents at `BACKUP_KYC_AT`
  (`/backups/kyc-docs/kyc-docs-<UTC>.tar.gz.age`), 14-day retention.

Failures are loud: any failing stage (pg_dump, tar, gzip, encryption, a
missing end-of-dump trailer) deletes the partial file, logs `ERROR:` and exits
non-zero; a failed run is retried hourly. The container healthcheck reports
the **age of the last successful backup** (unhealthy after 26 h), and each run
writes `payment_backup_last_success_timestamp_seconds{kind}` (and friends) for
node-exporter's textfile collector — the monitoring overlay alerts on stale or
failed backups and on a missed off-box copy.

**Encryption.** `secrets/backup_recipients` decides: age recipients (default —
`deploy.sh` generates a key pair on first run) or an ASCII-armoured OpenPGP
public key. Better: generate the key **on your workstation** and put only the
public part on the server:

```bash
age-keygen -o ~/payment-backup.key                  # keep this file off the server
age-keygen -y ~/payment-backup.key > deploy/secrets/backup_recipients
# or OpenPGP: gpg --armor --export ops@example.com > deploy/secrets/backup_recipients
```

`BACKUP_ENCRYPTION=none` is refused for production (allowed on the LAN box).

**Off-box copy.** A backup on the same disk as the database is not a backup.
Put an rclone remote in `secrets/backup_rclone_conf` and set
`BACKUP_OFFSITE_REMOTE=<remote>:<bucket>/<dir>` in `.env`; every backup (already
encrypted) is copied right after it is written. Set retention as a bucket
lifecycle rule. Example `backup_rclone_conf`:

```ini
[offsite]
type = s3
provider = Other
endpoint = https://s3.eu-central-003.backblazeb2.com
access_key_id = ...
secret_access_key = ...
```

To encrypt a file by hand, keep the compression inside:
`gpg --encrypt -r ops@example.com < dump.sql.gz > dump.sql.gz.gpg`.

**Restore drill** — run monthly from cron, after any change to backups, and on
a machine off the box with files fetched back from the off-box target:

```bash
./restore-drill.sh                                 # newest dump + KYC archive in the volume
./restore-drill.sh -i ~/payment-backup.key -k ~/trusted_keys \
    --kyc kyc-docs-<UTC>.tar.gz.age payment-<UTC>.sql.gz.age   # files fetched back
```

It restores into a throwaway Postgres on tmpfs (the live stack is untouched)
and checks: **freshness** (refuses a dump or archive older than `--max-age`,
default 30 h), **format** (gzip / plain SQL / pg_dump custom, optionally inside
age or gpg — detected from the bytes), **restore** with `ON_ERROR_STOP`,
**shape**, **conservation** (every currency sums to zero), **integrity** (every
balance equals the signed sum of its entries), **chain** (`payment-workers
verify-chain` from the workers image against the trusted public keys — an
attacker who rewrote the database and re-signed it with their own key fails
here) and the **KYC archive** (decrypts, lists completely, and contains every
document cited by a KYC submission made before it). Exit 0 = PASS, 1 = FAIL,
3 = passed with checks skipped (`--skip-chain`, `--kyc none`). CI runs it end
to end on every push (`scripts/ci-restore-drill.sh`), including a tampered
dump that only the chain verification can catch.

**Disaster recovery** (new box): copy `deploy/` and the off-box secrets
(`postgres_password`, the four `pg_*_password` files, `worker_signing_key`,
`worker_trusted_public_keys`, `biometric_template_key`, `jwt_secret`), start
only the database with a fresh volume, restore as the schema owner, then deploy:

```bash
docker compose up -d --wait postgres          # fresh volume: 10-roles.sh creates the roles
age -d -i ~/payment-backup.key payment-<UTC>.sql.gz.age | gunzip \
  | docker compose exec -T postgres psql -v ON_ERROR_STOP=1 -U payment_owner -d payment
./deploy.sh
```

Restore the KYC archive into the `kycdocs` volume the same way
(`age -d … | docker run -i --rm -v payment_kycdocs:/d -w /d alpine tar -xzf -`, then
`chown -R 65532:65532`).

### WAL archiving (point-in-time recovery)

Nightly dumps bound your loss to a day. For minutes, enable the opt-in overlay
`docker-compose.wal-archive.yml`: Postgres is rebuilt with pgBackRest
(`Dockerfile.postgres`), WAL is shipped continuously to an off-box
S3-compatible bucket, encrypted client-side. Steps are at the top of that file;
the configuration template is `pgbackrest/pgbackrest.conf.example`. With the
monitoring overlay, `WalArchiveFailing`/`WalArchiveStale` page on archiving
errors.

## Admin console and the admin allowlist

Caddy serves the console at `https://<SITE_ADDRESS>/admin/` from the
`payment-console` image (same origin as the API, so no CORS). Sign in with an
admin account. `ADMIN_ALLOW_CIDR` (required) restricts **every admin-capable
path** — the console and the admin API: `/v1/admin/*` (deposit approvals,
terminals, users, KYC queue, FX rates), `/v1/deposits*` and
`/v1/kyc/submissions/*/approve|reject` — to the listed IPs/CIDRs; everyone
else gets `403 {"error":{"code":"forbidden",…}}` at Caddy. Matching uses
Caddy's normalised path (case, `//`, `..` and percent-encoding cannot slip
past). New admin endpoints must live under `/v1/admin/` or be added to
`@admin_denied` in the Caddyfile. The `/admin` response policy is a strict CSP
without inline scripts or styles, cached assets for a year (content-hashed) and
an always-revalidated shell.

## Monitoring (optional overlay)

```bash
# in deploy/.env
COMPOSE_FILE=docker-compose.prod.yml:docker-compose.monitoring.yml
GRAFANA_PASSWORD=...             # required
ALERT_WEBHOOK_URL=https://...    # required: ntfy / Slack bridge / PagerDuty ...
ALERT_HEARTBEAT_URL=https://...  # required: external dead man's switch (healthchecks.io, Cronitor, ...)
./deploy.sh
```

Adds promtail → Loki (30-day retention, healthchecked), Prometheus (30 d /
2 GB) scraping both binaries, **node-exporter** (disk, memory, and the backup
textfile metrics) and **postgres-exporter** (as `payment_monitor`),
Alertmanager posting to your webhook, and Grafana on `127.0.0.1:3000` (reach it
via `ssh -L 3000:localhost:3000 user@server`).

**Dead man's switch.** Everything runs on one box, so a dead host cannot page
by itself. The always-firing `Watchdog` alert is routed only to
`ALERT_HEARTBEAT_URL` (about once a minute); configure that external check to
page you when pings stop (e.g. period 5 min, grace 5 min).

Alerts (rules + unit tests in `monitoring/`): reconciliation failed, chain
verification failed or stale, full reconciliation stale, no worker leader /
split brain, unsealed backlog ageing (an idle ledger stays quiet), outbox lag,
worker errors, API 5xx rate > 1 %, mean/p99 latency, pool exhausted, request
timeouts, refresh-token replay spikes, unusually large deposits / deposit
volume (read from the ledger; tune the thresholds in `prometheus-rules.yml`),
backup stale / failed / not copied off-box, disk low / critical / filling,
memory low, Postgres down, WAL archiving failing/stale, container restarts,
scrape targets down.

## LAN box (dev/staging only — plain HTTP, never real money)

`docker-compose.lan.yml` keeps the same hardened stack but publishes **Caddy**
on `LAN_BIND_ADDRESS` (required): the console on `:8088` and the API on
`:8099`, both plain HTTP (another application owns 80/443 there). The API port
goes through Caddy too, so the admin allowlist and per-replica health checks
apply and `TRUST_PROXY=true` stays. The box is too small to compile Rust, so
build on the dev machine and ship images:

```bash
# dev box
cd deploy && docker compose build
docker save payment-system:<sha> payment-console:<sha> payment-backup:<sha> | ssh box 'docker load'
rsync -a --exclude secrets --exclude .env --exclude app.env ./ box:~/payment-deploy/
# box: ~/payment-deploy/.env has COMPOSE_FILE=docker-compose.lan.yml,
#      LAN_BIND_ADDRESS=192.168.1.156, ADMIN_ALLOW_CIDR=192.168.1.0/24
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

- [x] Binaries refuse to boot on dev defaults outside `APP_ENV=dev`; config typos refuse to boot; `deploy.sh` refuses unsafe settings.
- [x] Secrets are files (0640, group `SECRETS_GID`, directory 0700), never in `.env`, `app.env`, images or `docker inspect`. *(Upgrade path: Vault.)*
- [x] The app runs as `payment_app`: no DDL, no ledger rewrites, cannot disable the append-only triggers; migrations run as `payment_owner`; backups read as `payment_backup`.
- [x] Only Caddy is internet-facing; the admin console **and admin API** are IP-allowlisted; DB/Redis/NATS/metrics are on internal networks; Redis and NATS require authentication.
- [x] Distroless + nonroot app image; non-root Caddy, Postgres, Redis and backup sidecars; read-only root filesystems; all capabilities dropped; `no-new-privileges`.
- [x] Images pinned by digest, CI actions by commit SHA; `cargo deny`, `npm audit`, secret scan (full history) and an image vulnerability scan in CI.
- [x] Every admin action is written to `admin_actions`; deposits need two admins.
- [x] Encrypted backups, a pre-deploy dump before every rollout, staleness alerts, an end-to-end restore drill in CI.
- [ ] Put the server behind a firewall; allow only 80/443 (+ SSH).
- [ ] Use a domain so Caddy enables HTTPS — never serve real traffic on `:80`.
- [ ] Configure the off-box copy (`BACKUP_OFFSITE_REMOTE`) and run `restore-drill.sh` monthly, also off the box.
- [ ] Move `backup_age_identity` off the server (or generate the backup key off it).
- [ ] Enable the monitoring overlay with an external heartbeat.
- [ ] Enable WAL archiving before there is real money in the ledger.

## The upgrade path to HA (when one box isn't enough)

This layout maps 1:1 onto the DESIGN.md §12 target:

1. **Postgres HA first**: a 3-node Patroni + etcd cluster; point
   `secrets/database_url` / `migration_database_url` at its VIP (session mode —
   the workers' leader election uses advisory locks). *Nothing in the app changes.*
2. **App tier to k3s/k8s**: the same image + `app.env` become a Deployment (API,
   N replicas) + a worker Deployment (leader-elected, so 2 replicas give a hot
   standby); Caddy → an Ingress with the same admin allowlist. The
   `healthcheck` subcommands become the liveness/readiness probes.
3. **Secrets to Vault**, GitOps via Argo CD.
4. **Scale writes** only if needed: swap `PostgresLedger` for
   `TigerBeetleLedger` behind the existing `LedgerStore` trait. One box does
   ~3 000 fee-bearing transfers/s today.
