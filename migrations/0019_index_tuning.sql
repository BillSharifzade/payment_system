-- Index tuning (review 2026-09-20).
--
-- Plain (transactional) DDL on purpose: CREATE/DROP INDEX CONCURRENTLY cannot
-- run inside a transaction and DEADLOCKS against a second replica waiting on
-- the migrator's advisory lock (the waiter's snapshot blocks the concurrent
-- build). These tables are small, so the brief exclusive lock is milliseconds.
-- Any future index on a LARGE ledger table must be built by hand with
-- CONCURRENTLY from a single session, then recorded as a no-op migration.

-- `idx_entries_account (account_id)` was strictly subsumed by
-- `idx_entries_account_statement (account_id, created_at DESC, id DESC)` (same
-- leading column), so every entry insert was maintaining it for nothing.
DROP INDEX IF EXISTS idx_entries_account;

-- Outbox retention deletes `sent_at IS NOT NULL AND sent_at < cutoff`, which the
-- partial unsent index cannot serve; without this every prune pass sequentially
-- scanned up to a week of events.
CREATE INDEX IF NOT EXISTS idx_outbox_sent ON outbox (sent_at) WHERE sent_at IS NOT NULL;

-- Incremental reconciliation re-derives only the accounts whose balance moved
-- since the last healthy pass (see workers::reconcile), keyed on updated_at.
CREATE INDEX IF NOT EXISTS idx_balances_updated ON balances (updated_at);
