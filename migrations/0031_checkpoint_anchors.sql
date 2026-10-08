-- External anchors of checkpoint hashes (DESIGN.md §6.4).
--
-- A checkpoint's Ed25519 signature proves only that the key holder wrote it,
-- and the key holder can re-sign a rewritten history. An anchor commits the
-- checkpoint hash to a witness outside the system — an RFC 3161 timestamp
-- authority or OpenTimestamps (Bitcoin) — and since each checkpoint hash
-- chains every earlier one, anchoring the newest checkpoint anchors all
-- history before it. The verifier fails the chain when an anchored
-- checkpoint's hash changes, whoever re-signed it.
--
-- The witnessed datum is the 32 bytes of checkpoint_hash; both kinds
-- timestamp its SHA-256, so the stored proofs verify with stock tools:
--   rfc3161   openssl ts -verify -data <hash bytes> -in <proof> -CAfile <TSA roots>
--   ots       ots verify -f <hash bytes file> <proof>   (needs a Bitcoin node)
CREATE TABLE checkpoint_anchors (
    id               UUID PRIMARY KEY,                 -- v7: insertion order
    checkpoint_seq   BIGINT NOT NULL REFERENCES checkpoints (seq),
    checkpoint_hash  BYTEA NOT NULL CHECK (octet_length(checkpoint_hash) = 32),
    kind             TEXT NOT NULL CHECK (kind IN ('rfc3161', 'ots')),
    -- The TSA URL, or 'opentimestamps' (the calendars are named in the proof).
    witness          TEXT NOT NULL CHECK (char_length(witness) BETWEEN 1 AND 2048),
    -- 'pending': an OpenTimestamps calendar's promise, not yet in a block.
    status           TEXT NOT NULL CHECK (status IN ('complete', 'pending')),
    -- The full TimeStampResp DER, or the .ots file.
    proof            BYTEA NOT NULL CHECK (octet_length(proof) BETWEEN 1 AND 65536),
    attested_at      TIMESTAMPTZ,                      -- RFC 3161 genTime
    bitcoin_height   BIGINT CHECK (bitcoin_height >= 0),
    -- A completed OpenTimestamps proof is a NEW row naming the pending one.
    upgrades         UUID REFERENCES checkpoint_anchors (id),
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (
        (kind = 'rfc3161' AND status = 'complete' AND attested_at IS NOT NULL
             AND bitcoin_height IS NULL AND upgrades IS NULL)
     OR (kind = 'ots' AND status = 'pending' AND attested_at IS NULL
             AND bitcoin_height IS NULL AND upgrades IS NULL)
     OR (kind = 'ots' AND status = 'complete' AND attested_at IS NULL
             AND bitcoin_height IS NOT NULL AND upgrades IS NOT NULL)
    )
);

-- "What did this witness last anchor?" and the verifier's per-checkpoint join.
CREATE INDEX idx_checkpoint_anchors_witness ON checkpoint_anchors (kind, witness, checkpoint_seq);
CREATE INDEX idx_checkpoint_anchors_checkpoint ON checkpoint_anchors (checkpoint_seq);
-- One completion per pending proof, even if two leaders race.
CREATE UNIQUE INDEX idx_checkpoint_anchors_upgrades ON checkpoint_anchors (upgrades)
    WHERE upgrades IS NOT NULL;

-- Append-only like the ledger (0025): reject_history_rewrite() refuses any
-- UPDATE, DELETE or TRUNCATE, statement-level.
DROP TRIGGER IF EXISTS checkpoint_anchors_append_only ON checkpoint_anchors;
CREATE TRIGGER checkpoint_anchors_append_only
    BEFORE UPDATE OR DELETE OR TRUNCATE ON checkpoint_anchors
    FOR EACH STATEMENT EXECUTE FUNCTION reject_history_rewrite();

-- Least privilege (0027's rule for new append-only tables): the runtime role
-- reads and inserts anchors, never rewrites them. A no-op without the role
-- (dev/test clusters).
DO $grants$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'payment_app') THEN
        EXECUTE 'GRANT SELECT, INSERT ON checkpoint_anchors TO payment_app';
        EXECUTE 'REVOKE UPDATE, DELETE, TRUNCATE ON checkpoint_anchors FROM payment_app';
    END IF;
END $grants$;
