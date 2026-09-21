-- Schema hardening + write-amplification fixes (review 2026-09-20).

-- `balances` is rewritten on every post and none of its updated columns is
-- indexed, so updates are HOT-eligible — but only when the page has free space.
-- A low fillfactor on this tiny, hottest table keeps updates in-page and cuts
-- PK-index bloat and vacuum pressure.
ALTER TABLE balances SET (fillfactor = 50);

-- Defence in depth for the no-overdraft rule, which was enforced only in Rust:
-- `min_raw` is 0 for user wallets and NULL (unbounded) for system accounts, and
-- the CHECK makes any code path — ops scripts, a second backend — unable to
-- drive a wallet negative. Adding the constraint validates existing rows, so a
-- ledger that already violated it fails loudly here instead of silently.
ALTER TABLE balances ADD COLUMN IF NOT EXISTS min_raw BIGINT;
UPDATE balances b SET min_raw = 0
FROM accounts a
WHERE a.id = b.account_id AND a.account_type = 'user_wallet' AND b.min_raw IS NULL;
ALTER TABLE balances DROP CONSTRAINT IF EXISTS balances_min_raw_check;
ALTER TABLE balances ADD CONSTRAINT balances_min_raw_check
    CHECK (min_raw IS NULL OR raw_minor >= min_raw);

-- `entries.currency → currencies(code)` took a FOR KEY SHARE lock on the single
-- 'TJS' row for every entry inserted (multixact churn on one hot tuple at high
-- TPS). The FK is redundant: `accounts.currency` already references
-- `currencies`, and storage rejects any entry whose currency differs from its
-- account's. The column and the application check stay.
ALTER TABLE entries DROP CONSTRAINT IF EXISTS entries_currency_fkey;

-- Refresh-token rotation now records which token replaced which, so a client
-- that lost the rotation response can re-present the just-rotated token inside
-- a short grace window and receive a fresh pair (the unclaimed successor is
-- revoked), while a genuine replay still revokes the whole family.
ALTER TABLE refresh_tokens ADD COLUMN IF NOT EXISTS successor_id UUID;
-- A token minted by the grace path is marked, so the grace path can be taken
-- at most once per rotation: presenting a token whose successor was itself a
-- grace product is a genuine double use and revokes the family.
ALTER TABLE refresh_tokens ADD COLUMN IF NOT EXISTS grace_issued BOOLEAN NOT NULL DEFAULT false;

-- Who did what: an append-only audit trail of privileged (admin) actions. The
-- ledger records *that* a wallet was funded; this records *which admin* did it.
CREATE TABLE IF NOT EXISTS admin_actions (
    id          UUID PRIMARY KEY,
    admin_id    UUID NOT NULL REFERENCES users(id),
    action      TEXT NOT NULL,          -- 'deposit', 'fx_rate.set', ...
    target      TEXT,                   -- wallet id, currency pair, user id ...
    details     JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_admin_actions_time ON admin_actions (created_at);
CREATE INDEX IF NOT EXISTS idx_admin_actions_admin_time ON admin_actions (admin_id, created_at);

-- The admin KYC queue pages oldest-first by keyset (status, created_at, id).
CREATE INDEX IF NOT EXISTS idx_kyc_submissions_status_created
    ON kyc_submissions (status, created_at, id);
