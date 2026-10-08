# Payment System

A fast, secure, double-entry e-money backend in Rust, with a native Android
customer app and a React admin console. See [`DESIGN.md`](./DESIGN.md) for the
architecture and the reasoning behind every decision, [`FRONTEND.md`](./FRONTEND.md)
for the clients, and [`deploy/README.md`](./deploy/README.md) to run it in production.

## Layout

| Path | What it is |
|---|---|
| `crates/money` | Currency-safe integer money. No floats, ever. *(pure)* |
| `crates/ledger` | Double-entry core: accounts, transactions, balancing and conservation invariants. *(pure)* |
| `crates/storage` | `PostgresLedger` — durable, row-locked, concurrency-safe posting (five round trips per transfer). |
| `crates/auth` | Argon2id passwords, JWT access tokens, hashed rotating refresh tokens. *(pure)* |
| `crates/crypto` | SHA-256 Merkle trees + Ed25519 for tamper-evident checkpoints. *(pure)* |
| `crates/biometric` | Fingerprint payments: template validation, AES-GCM sealing, match-decision policy, HTTP adapter to a matching engine. *(pure)* |
| `crates/api` | axum HTTP server → binary `payment-server`. |
| `crates/workers` | sealer, verifier, reconciliation, outbox → NATS JetStream relay → binary `payment-workers`. |
| `migrations/` | sqlx migrations, embedded into the binaries and applied on startup. |
| `console/` | React/TypeScript admin console (served at `/admin`). |
| `mobile/` | Kotlin/Jetpack Compose customer app. |
| `deploy/` | Dockerfiles, hardened Compose stack, backups, monitoring, restore drill. |

The pure crates have **zero** dependency on a database or HTTP, so their
invariants are tested exhaustively in isolation (incl. property/simulation tests).

## Quickstart (dev box)

```bash
# 1. Start Postgres 16, NATS (JetStream) and Redis.
docker compose up -d
# The dev box publishes Postgres on host port 5439 (docker-compose.override.yml).

# 2. Build and run the server (applies migrations on startup).
DATABASE_URL=postgres://payment:payment_dev_pw@localhost:5439/payment \
  cargo run -p api --bin payment-server
# → listening on 0.0.0.0:8080 (override with BIND_ADDR); metrics on 127.0.0.1:9100

# 3. Optionally the workers (sealing, reconciliation, outbox relay).
DATABASE_URL=postgres://payment:payment_dev_pw@localhost:5439/payment \
NATS_URL=nats://localhost:4222 cargo run -p workers --bin payment-workers
```

### Try it

```bash
BASE=http://localhost:8080
J='content-type: application/json'
PSQL='docker compose exec -T postgres psql -U payment -d payment -qtAc'
reg() { curl -s -XPOST $BASE/v1/auth/register -H "$J" -d "{\"phone\":\"$1\",\"password\":\"password123\"}"; }

# Two customers and two operators. Every customer gets a TJS wallet at registration.
ALICE=$(reg +992900000001); BOB=$(reg +992900000002)
OPS1=$(reg +992900000091);  OPS2=$(reg +992900000092)
tok() { echo "$1" | jq -r .access_token; }
A_TOK=$(tok "$ALICE"); B_TOK=$(tok "$BOB"); O1_TOK=$(tok "$OPS1"); O2_TOK=$(tok "$OPS2")
A_WALLET=$(curl -s $BASE/v1/wallets -H "Authorization: Bearer $A_TOK" | jq -r '.[0].id')
B_WALLET=$(curl -s $BASE/v1/wallets -H "Authorization: Bearer $B_TOK" | jq -r '.[0].id')

# Operators are flipped by SQL; customers become KYC level 1 as a reviewer would.
$PSQL "UPDATE users SET is_admin=true WHERE phone IN ('992900000091','992900000092')"
$PSQL "UPDATE users SET kyc_level=1 WHERE phone IN ('992900000001','992900000002')"

# Deposits are dual-control: one operator requests (202 pending_approval), a
# DIFFERENT operator approves, and only then is the ledger posted. The
# Idempotency-Key (a UUID) is the deposit id and the eventual transaction id.
DEP=$(uuidgen)
curl -s -XPOST $BASE/v1/deposits -H "$J" -H "Authorization: Bearer $O1_TOK" -H "Idempotency-Key: $DEP" \
  -d "{\"user_account\":\"$A_WALLET\",\"amount_minor\":10000,\"currency\":\"TJS\"}"   # → pending_approval
curl -s -XPOST $BASE/v1/admin/deposits/$DEP/approve -H "Authorization: Bearer $O2_TOK"           # → posted

# Transfer 35.00 TJS to Bob. A retry with the same key can never post twice.
curl -s -XPOST $BASE/v1/transfers -H "$J" -H "Authorization: Bearer $A_TOK" \
  -H "Idempotency-Key: $(uuidgen)" \
  -d "{\"from_account\":\"$A_WALLET\",\"to_account\":\"$B_WALLET\",\"amount_minor\":3500,\"currency\":\"TJS\"}"

curl -s $BASE/v1/accounts/$A_WALLET/balance -H "Authorization: Bearer $A_TOK"   # → 65.00 TJS
curl -s $BASE/ready                                                              # → {"status":"ready"}

# Gave up on an unsettled payment? Void its key first — afterwards it can never post.
curl -s -XPOST $BASE/v1/transactions/$(uuidgen)/void -H "Authorization: Bearer $A_TOK"   # → voided

# Fingerprint payment (DESIGN.md §20). Bob enrols a finger once (template from the
# scanner SDK, base64). An operator registers Alice's checkout terminal (its key is
# shown once). Alice opens a 2.00 TJS check; Bob pays it with his finger, the cashier
# keying in his phone number (1:1 verification). Dev mode matches templates exactly.
FP=$(head -c 64 /dev/urandom | base64 -w0)
curl -s -XPOST $BASE/v1/biometric/fingerprints -H "$J" -H "Authorization: Bearer $B_TOK" \
  -d "{\"finger\":2,\"format\":\"raw\",\"template\":\"$FP\",\"consent\":true}"
A_ID=$(echo "$ALICE" | jq -r .user_id)
TKEY=$(curl -s -XPOST $BASE/v1/admin/terminals -H "$J" -H "Authorization: Bearer $O1_TOK" \
  -d "{\"merchant_user_id\":\"$A_ID\",\"label\":\"till 1\"}" | jq -r .api_key)
CHECK=$(uuidgen)
curl -s -XPOST $BASE/v1/checks -H "$J" -H "Authorization: Bearer $A_TOK" -H "Idempotency-Key: $CHECK" \
  -d "{\"account\":\"$A_WALLET\",\"amount_minor\":200,\"description\":\"bread\"}"
curl -s -XPOST $BASE/v1/checks/$CHECK/pay/fingerprint -H "$J" -H "Authorization: Bearer $A_TOK" \
  -H "X-Terminal-Key: $TKEY" -H "Idempotency-Key: $(uuidgen)" \
  -d "{\"format\":\"raw\",\"template\":\"$FP\",\"payer_phone\":\"+992900000002\"}"   # → posted
```

Amounts are always **integer minor units** (diram for TJS): `10000` = 100.00 TJS.
Errors have the shape `{"error":{"code","message","request_id"}}`; a 503
`retry_later` or 504 `timeout` means "outcome unknown — retry with the same key".

## Tests

```bash
# Pure unit + property/simulation tests (no infrastructure).
cargo test --workspace

# Integration lanes against the running dev services, one crate at a time
# (they share one database and a global outbox).
export DATABASE_URL=postgres://payment:payment_dev_pw@localhost:5439/payment
export REDIS_URL=redis://localhost:6379 NATS_URL=nats://localhost:4222
cargo test -p storage -- --ignored
cargo test -p api -- --ignored
cargo test -p workers -- --ignored --test-threads=1

# Lint gate used in CI.
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings

# Console and mobile.
(cd console && npm ci && npm run lint && npm test && npm run build)
(cd mobile && ./gradlew :core:test :app:testDevDebugUnitTest)
```

CI (`.github/workflows/ci.yml`) runs all of the above plus `cargo deny`,
`npm audit` and both container image builds — it needs the repository pushed to
GitHub to execute.

## Production

`deploy/README.md`: one command (`deploy/deploy.sh`) builds SHA-tagged images
and starts the hardened Compose stack — Caddy (TLS, `/admin`, `/ready` health
checks), the two binaries, Postgres, Redis, NATS JetStream, backup sidecars,
optional monitoring (Loki/Prometheus/Alertmanager/Grafana) and optional WAL
archiving. Secrets are files, config typos refuse to boot, every service is
health-checked, capability-dropped and log-rotated.

Since the hardening pass:

- **`ADMIN_ALLOW_CIDR` is required.** Caddy applies it to every admin-capable
  path (`/admin`, `/v1/admin/*`, `/v1/deposits*`, KYC approve/reject);
  `0.0.0.0/0 ::/0` is the explicit, loudly-warned "anyone" setting.
- **Least-privilege database.** `payment_owner` owns the schema and runs
  migrations (`MIGRATION_DATABASE_URL_FILE`); the server and workers run as
  `payment_app`, which can neither bypass the append-only triggers nor
  UPDATE/DELETE ledger history; `payment_backup` dumps read-only.
- **Segmented networks;** Redis and NATS require passwords; Postgres runs
  without capabilities. Images are pinned by digest (`scripts/image-digest.sh`).
- **Encrypted backups** (age or GPG, optional off-box copy via rclone), a
  pre-deploy dump before every rollout, freshness alerts, and a restore drill
  that verifies the signed checkpoint chain — exercised in CI on every push.
- **Existing installs** upgrade once with `deploy/upgrade-hardening.sh`
  (idempotent; moves secrets into files, creates the roles, transfers
  ownership), then `deploy/deploy.sh`.
