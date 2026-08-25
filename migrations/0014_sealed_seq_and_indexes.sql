-- Fix the checkpoint sealer's commit-order race (audit 2026-07-03).
--
-- `transactions.seq` is a BIGSERIAL assigned at INSERT time, but rows COMMIT in
-- a different order. The sealer can only see committed rows, so a still-in-
-- flight transaction with a lower seq than a sealed checkpoint's to_txn_seq
-- could commit later INSIDE the sealed range — it would then (a) never be
-- covered by any checkpoint and (b) make verify_chain recompute a different
-- Merkle root, raising a FALSE ChainBroken tamper alarm.
--
-- The fix: the sealer (a single writer by design) assigns its own dense,
-- commit-ordered `sealed_seq` to rows it can actually see, in the same database
-- transaction that writes the checkpoint. Checkpoint ranges and verification
-- run over `sealed_seq`, whose covered set can never change after sealing.
ALTER TABLE transactions ADD COLUMN sealed_seq BIGINT;
CREATE UNIQUE INDEX idx_transactions_sealed_seq
    ON transactions (sealed_seq) WHERE sealed_seq IS NOT NULL;

-- Backfill: checkpoints written before this migration covered `seq BETWEEN
-- from_txn_seq AND to_txn_seq`. Preserve their meaning under the new scheme by
-- setting sealed_seq = seq for every transaction they covered (ranges verified
-- against the same set before, so old checkpoints keep verifying).
UPDATE transactions
SET sealed_seq = seq
WHERE seq <= (SELECT COALESCE(MAX(to_txn_seq), 0) FROM checkpoints);

-- The sealer's work queue: committed-but-unsealed transactions, scanned in
-- insert order. Partial, so it stays tiny (only unsealed rows) no matter how
-- large the ledger grows.
CREATE INDEX idx_transactions_unsealed
    ON transactions (seq) WHERE sealed_seq IS NULL;

-- AML screening runs two scans per transfer over this exact shape
-- (account_id + direction='debit' + created_at window). The account-only index
-- degraded linearly with a wallet's history.
CREATE INDEX idx_entries_account_debit_time
    ON entries (account_id, created_at) WHERE direction = 'debit';
