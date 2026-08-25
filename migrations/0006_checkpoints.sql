-- Tamper-evidence checkpoints (DESIGN.md §6).
--
-- The correctness path (double-entry, ACID) is already complete by the time a
-- transaction commits. This adds an ASYNCHRONOUS, batched cryptographic seal so
-- the ledger's history is tamper-EVIDENT without putting any hashing on the
-- money hot path.

-- A monotonic sequence over transactions, so checkpoints can cover deterministic
-- ranges. BIGSERIAL backfills existing rows and is NOT NULL going forward.
ALTER TABLE transactions ADD COLUMN seq BIGSERIAL;
CREATE UNIQUE INDEX idx_transactions_seq ON transactions (seq);

-- Each checkpoint commits a contiguous range of transactions to one Merkle root,
-- chains to the previous checkpoint's hash, and is Ed25519-signed. Altering any
-- historical transaction changes its Merkle root, which breaks the signed chain.
CREATE TABLE checkpoints (
    id                    UUID PRIMARY KEY,
    seq                   BIGINT NOT NULL UNIQUE,   -- 1, 2, 3, ...
    from_txn_seq          BIGINT NOT NULL,
    to_txn_seq            BIGINT NOT NULL,
    txn_count             BIGINT NOT NULL,
    merkle_root           BYTEA NOT NULL,
    prev_checkpoint_hash  BYTEA NOT NULL,           -- 32 zero bytes for the genesis checkpoint
    checkpoint_hash       BYTEA NOT NULL,
    signature             BYTEA NOT NULL,           -- Ed25519, 64 bytes
    public_key            BYTEA NOT NULL,           -- 32 bytes, for auditor convenience
    created_at            TIMESTAMPTZ NOT NULL DEFAULT now()
);
