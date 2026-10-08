-- Incremental reconciliation without re-reading history (workers::reconcile).
--
-- Re-deriving a touched account's balance from ALL of its entries made every
-- pass O(ledger): the fee and settlement shards are touched by nearly every
-- transfer. Instead the reconciler keeps, per account, the signed sum of its
-- entries in transactions sealed at or below one global watermark. sealed_seq
-- is assigned densely in commit order and never changes afterwards (0025), so
-- folding the range (watermark, max sealed_seq] into these sums is exact, and a
-- pass costs O(transactions since the last pass):
--
--   balance(a) = reconciled_sums(a) + entries(a) in transactions sealed after
--                the watermark or not yet sealed
--
-- Starting from watermark 0 with no rows, the worker folds an existing ledger
-- in bounded chunks after deploy; the periodic full pass re-derives these sums
-- from the entries, so a corrupted row here is detected rather than trusted.
CREATE TABLE IF NOT EXISTS reconciled_sums (
    account_id  UUID PRIMARY KEY REFERENCES accounts(id),
    sum_minor   BIGINT NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS reconcile_watermark (
    id                  BOOLEAN PRIMARY KEY DEFAULT true CHECK (id),
    through_sealed_seq  BIGINT NOT NULL CHECK (through_sealed_seq >= 0),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
INSERT INTO reconcile_watermark (id, through_sealed_seq) VALUES (true, 0)
ON CONFLICT (id) DO NOTHING;
