-- The posting hot path (perf pass 2026-10; measurements in crates/loadtest/README.md):
-- HOT balance updates, accounts that cannot be retyped, 64 shards per system account.
--
-- Balance updates are HOT again. `idx_balances_updated (updated_at)` (0019) served the incremental reconciliation of that
-- time; since 0026 it folds sealed transactions past a watermark and never reads
-- balances.updated_at. But every post rewrites updated_at, and an UPDATE that changes an
-- indexed column cannot be HOT: each balance update inserted fresh entries into both balance
-- indexes and left a dead tuple for vacuum (a run of 160 000 updates measured 0 HOT), and each
-- of those frequent vacuums invalidated the cached plans of every backend. Without the index
-- the fillfactor-50 pages (0020) absorb the updates in place. Plain DROP INDEX: balances is
-- small, the exclusive lock is milliseconds (see 0019 on why not CONCURRENTLY).
DROP INDEX IF EXISTS idx_balances_updated;

-- Accounts are immutable in what the ledger trusts: storage reads a system account's type and
-- currency once and caches them, and every entry carries the currency its account had when it
-- was posted. Neither the id, the type nor the currency of an account may change (owner and
-- creation time are not the ledger's business).
CREATE OR REPLACE FUNCTION accounts_reject_retype() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'account %: id, account_type and currency are immutable', OLD.id
        USING ERRCODE = 'restrict_violation';
END $$;

DROP TRIGGER IF EXISTS accounts_immutable ON accounts;
CREATE TRIGGER accounts_immutable
    BEFORE UPDATE ON accounts
    FOR EACH ROW
    WHEN (OLD.id IS DISTINCT FROM NEW.id
          OR OLD.account_type IS DISTINCT FROM NEW.account_type
          OR OLD.currency IS DISTINCT FROM NEW.currency)
    EXECUTE FUNCTION accounts_reject_retype();

-- 64 shards per hot system account instead of 16 (0013, 0021). A shard is held from its
-- additive update until the commit; at 32 posts in flight two of them met on a shard often
-- enough that the fee-shard UPDATE averaged a 4 ms wait. Same id scheme, base + i:
--   0x1000 settlement TJS   0x1100 settlement USD   0x2000 fee revenue TJS
--   0x3000 FX position TJS  0x3100 FX position USD          (api: payments::SHARD_COUNT)
INSERT INTO accounts (id, account_type, currency, owner_user_id)
SELECT ('00000000-0000-0000-0000-' || lpad(to_hex(base + g), 12, '0'))::uuid, t, cur, NULL
FROM (VALUES
        (4096,  'system_settlement',   'TJS'),
        (4352,  'system_settlement',   'USD'),
        (8192,  'system_fee_revenue',  'TJS'),
        (12288, 'system_fx_gain_loss', 'TJS'),
        (12544, 'system_fx_gain_loss', 'USD')
     ) AS s(base, t, cur)
CROSS JOIN generate_series(16, 63) AS g
ON CONFLICT (id) DO NOTHING;

INSERT INTO balances (account_id, raw_minor, version, min_raw)
SELECT ('00000000-0000-0000-0000-' || lpad(to_hex(base + g), 12, '0'))::uuid, 0, 0, NULL
FROM (VALUES (4096), (4352), (8192), (12288), (12544)) AS s(base)
CROSS JOIN generate_series(16, 63) AS g
ON CONFLICT (account_id) DO NOTHING;
