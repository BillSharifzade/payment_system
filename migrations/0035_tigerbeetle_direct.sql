-- TigerBeetle direct posting (TIGERBEETLE_POSTING=direct; crates/ledger-tigerbeetle
-- direct.rs, limits.rs, mirror.rs; DESIGN.md §15). Inert otherwise: nothing reads or writes
-- these with LEDGER_BACKEND=postgres or under hybrid posting.
--
-- A direct posting is committed by the cluster; the journal mirror writes its rows here
-- afterwards. tb_timestamp is the cluster timestamp of the posting's key (when it was
-- applied): NULL for every transaction Postgres committed itself. The mirror refuses to
-- journal a cluster posting whose id the journal already holds with another origin. Like
-- every other column, it can never change (transactions_seal_once compares whole rows).
ALTER TABLE transactions ADD COLUMN tb_timestamp BIGINT;

-- The mirror's position per cluster: every journaled transfer at or below this cluster
-- timestamp is in Postgres. A pass locks the row and moves it in the same transaction as the
-- rows it writes, which is what makes the mirror exactly-once and safe to run concurrently.
CREATE TABLE tb_mirror_watermark (
    cluster_id         UUID PRIMARY KEY,   -- the u128 cluster id
    through_timestamp  BIGINT NOT NULL CHECK (through_timestamp >= 0),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Which posting mode a cluster serves this database with. `payment-server
-- tigerbeetle-posting direct|hybrid` switches it with money movement stopped (the switch
-- seeds the AML holds of the last 24 h into the cluster, or drains the mirror on the way
-- back); a server or worker whose TIGERBEETLE_POSTING differs refuses to start. No row means
-- hybrid, except on a database without transactions, which takes the configured mode.
CREATE TABLE tb_posting (
    cluster_id   UUID PRIMARY KEY,
    posting      TEXT NOT NULL CHECK (posting IN ('hybrid', 'direct')),
    switched_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Least privilege (0027's rule for new tables): the runtime role mirrors and switches; it
-- may never forget either record.
DO $grants$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'payment_app') THEN
        EXECUTE 'GRANT SELECT, INSERT, UPDATE ON tb_mirror_watermark TO payment_app';
        EXECUTE 'REVOKE DELETE, TRUNCATE ON tb_mirror_watermark FROM payment_app';
        EXECUTE 'GRANT SELECT, INSERT, UPDATE ON tb_posting TO payment_app';
        EXECUTE 'REVOKE DELETE, TRUNCATE ON tb_posting FROM payment_app';
    END IF;
END $grants$;
