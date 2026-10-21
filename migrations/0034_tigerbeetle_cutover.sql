-- TigerBeetle cut-over record (crates/ledger-tigerbeetle/src/import.rs, deploy/README.md).
-- Inert with LEDGER_BACKEND=postgres.
--
-- `payment-server tigerbeetle-import` copies every balance into a cluster and records it here;
-- `payment-server tigerbeetle-rollback` rebuilds `balances` from the journal and stamps
-- rolled_back_at. A server or worker with LEDGER_BACKEND=tigerbeetle refuses to start on a
-- database whose balances were never imported into its cluster (any non-zero `balances` row
-- and no live record here), and on a cluster that was rolled back: Postgres moved money after
-- the rollback, so that cluster's balances are stale for good. A new cut-over needs a freshly
-- formatted cluster (a new cluster id).
CREATE TABLE tb_cutover (
    cluster_id        UUID PRIMARY KEY,   -- the u128 cluster id
    accounts          BIGINT NOT NULL CHECK (accounts >= 0),
    opening_balances  BIGINT NOT NULL CHECK (opening_balances >= 0),
    through_seq       BIGINT NOT NULL CHECK (through_seq >= 0),
    imported_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    rolled_back_at    TIMESTAMPTZ
);

-- Least privilege (0027's rule for new tables): the runtime role runs both commands; it may
-- record and stamp, never forget a cut-over.
DO $grants$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'payment_app') THEN
        EXECUTE 'GRANT SELECT, INSERT, UPDATE ON tb_cutover TO payment_app';
        EXECUTE 'REVOKE DELETE, TRUNCATE ON tb_cutover FROM payment_app';
    END IF;
END $grants$;
