# Payment System — Backend

A fast, secure, double-entry payment backend in Rust. See [`DESIGN.md`](./DESIGN.md)
for the full architecture and the reasoning behind every decision.

## Workspace layout

| Crate | What it is | Pure? |
|---|---|---|
| `crates/money` | Currency-safe integer money. No floats, ever. | ✅ pure |
| `crates/ledger` | Double-entry core: accounts, transactions, the balancing & conservation invariants. | ✅ pure |
| `crates/storage` | `PostgresLedger` — durable, row-locked, concurrency-safe persistence. | needs Postgres |
| `crates/api` | axum HTTP server (`payment-server`). | needs Postgres |

The pure crates have **zero** dependency on a database or HTTP, so their
invariants are tested exhaustively in isolation (incl. property/simulation tests).

## Quickstart

```bash
# 1. Start the dev database (Postgres 16).
docker compose up -d

# 2. Build and run the server (applies migrations on startup).
DATABASE_URL=postgres://payment:payment_dev_pw@localhost:5432/payment \
  cargo run -p api --bin payment-server
# → listening on 0.0.0.0:8080 (override with BIND_ADDR)
```

### Try it

```bash
BASE=http://localhost:8080

# Create a funding (system) account and two user wallets.
SETTLE=$(curl -s -XPOST $BASE/v1/accounts -H 'content-type: application/json' \
  -d '{"account_type":"system_settlement","currency":"TJS"}' | jq -r .id)
ALICE=$(curl -s -XPOST $BASE/v1/accounts -H 'content-type: application/json' \
  -d '{"account_type":"user_wallet","currency":"TJS"}' | jq -r .id)
BOB=$(curl -s -XPOST $BASE/v1/accounts -H 'content-type: application/json' \
  -d '{"account_type":"user_wallet","currency":"TJS"}' | jq -r .id)

# Deposit 100.00 TJS to Alice, then transfer 35.00 to Bob.
# Every money-moving call needs an Idempotency-Key (a UUID).
curl -s -XPOST $BASE/v1/deposits -H 'content-type: application/json' \
  -H "Idempotency-Key: $(uuidgen)" \
  -d "{\"settlement_account\":\"$SETTLE\",\"user_account\":\"$ALICE\",\"amount_minor\":10000,\"currency\":\"TJS\"}"

curl -s -XPOST $BASE/v1/transfers -H 'content-type: application/json' \
  -H "Idempotency-Key: $(uuidgen)" \
  -d "{\"from_account\":\"$ALICE\",\"to_account\":\"$BOB\",\"amount_minor\":3500,\"currency\":\"TJS\"}"

curl -s $BASE/v1/accounts/$ALICE/balance   # → 65.00 TJS
```

Amounts are always **integer minor units** (diram for TJS): `10000` = 100.00 TJS.

## Tests

```bash
# Pure unit + property/simulation tests (no database needed).
cargo test --workspace

# Integration tests against the running Postgres (incl. the concurrency / double-spend test).
DATABASE_URL=postgres://payment:payment_dev_pw@localhost:5432/payment \
  cargo test --workspace -- --include-ignored

# Lint gate used in CI.
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

## Production deployment

The dev `docker-compose.yml` is intentionally a single machine. The production
HA datacenter topology (Talos/Kubernetes, Patroni-managed Postgres, etc.) is a
separate, later concern — see `DESIGN.md` §12.
