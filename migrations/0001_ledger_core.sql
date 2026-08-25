-- Ledger core schema. Mirrors the pure `ledger` crate's model.
--
-- Design choices that matter:
--  * Amounts are integer minor units stored as BIGINT (no floats, ever). i64
--    minor units holds ~9.2e16 — for TJS (exponent 2) that is ~900 trillion
--    somoni, far beyond any national money supply. The Rust core computes in
--    i128 for safe intermediate arithmetic and converts down with a checked
--    cast on the way to the DB.
--  * `entries` and `transactions` are append-only. No UPDATE/DELETE — corrections
--    are new reversing transactions. (Enforced operationally + by DB role grants
--    later; the app code never issues such statements.)
--  * Balances are stored RAW and signed (credit +, debit -), so the master
--    invariant is simply: SUM(raw_minor) per currency = 0.

CREATE TABLE currencies (
    code        TEXT PRIMARY KEY CHECK (code ~ '^[A-Z]{3}$'),
    exponent    SMALLINT NOT NULL CHECK (exponent >= 0 AND exponent <= 8),
    name        TEXT NOT NULL
);

CREATE TABLE accounts (
    id            UUID PRIMARY KEY,
    account_type  TEXT NOT NULL CHECK (account_type IN (
                      'user_wallet',
                      'system_settlement',
                      'system_fee_revenue',
                      'system_fx_gain_loss',
                      'system_suspense'
                  )),
    currency      TEXT NOT NULL REFERENCES currencies(code),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Materialized balance, updated inside the same DB transaction as the entries.
-- `version` is an optimistic-concurrency guard / change counter.
CREATE TABLE balances (
    account_id  UUID PRIMARY KEY REFERENCES accounts(id),
    raw_minor   BIGINT NOT NULL DEFAULT 0,
    version     BIGINT NOT NULL DEFAULT 0,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE transactions (
    id          UUID PRIMARY KEY,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE entries (
    id              UUID PRIMARY KEY,
    transaction_id  UUID NOT NULL REFERENCES transactions(id),
    account_id      UUID NOT NULL REFERENCES accounts(id),
    direction       TEXT NOT NULL CHECK (direction IN ('debit', 'credit')),
    amount_minor    BIGINT NOT NULL CHECK (amount_minor > 0),
    currency        TEXT NOT NULL REFERENCES currencies(code),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_entries_account ON entries (account_id);
CREATE INDEX idx_entries_transaction ON entries (transaction_id);
