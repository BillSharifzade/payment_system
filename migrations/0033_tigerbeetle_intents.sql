-- TigerBeetle ledger backend (crates/ledger-tigerbeetle, DESIGN.md §15 Phase 2).
-- Inert with LEDGER_BACKEND=postgres: nothing reads or writes these tables then.
--
-- tb_intents: what Postgres decided for each reservation attempt. The posting
-- transaction inserts 'commit' in the same database transaction as the journal
-- entries; recovery inserts 'void' (ON CONFLICT DO NOTHING) for an attempt it
-- finds unsettled. The primary key serialises the two: whichever row exists is
-- the outcome, and TigerBeetle is made to match it (post or void). Rows are
-- facts and never change.
CREATE TABLE tb_intents (
    attempt_id      UUID PRIMARY KEY,   -- TigerBeetle reservation ids are attempt_id + leg
    transaction_id  UUID NOT NULL,      -- no FK: a 'void' tombstone may name an unclaimed id
    outcome         TEXT NOT NULL CHECK (outcome IN ('commit', 'void')),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

DROP TRIGGER IF EXISTS tb_intents_append_only ON tb_intents;
CREATE TRIGGER tb_intents_append_only
    BEFORE UPDATE OR DELETE OR TRUNCATE ON tb_intents
    FOR EACH STATEMENT EXECUTE FUNCTION reject_history_rewrite();

-- Recovery's scan position per cluster: every reservation at or below this
-- TigerBeetle timestamp is settled. Only ever moved forward (by the UPDATE's
-- WHERE through_timestamp < new); a regression merely re-scans.
CREATE TABLE tb_recovery_watermark (
    cluster_id         UUID PRIMARY KEY,   -- the u128 cluster id
    through_timestamp  BIGINT NOT NULL CHECK (through_timestamp >= 0),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Least privilege (0027's rule for new tables): the default privileges grant
-- payment_app full DML; take back what it must not do. A no-op without the role.
DO $grants$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'payment_app') THEN
        EXECUTE 'GRANT SELECT, INSERT ON tb_intents TO payment_app';
        EXECUTE 'REVOKE UPDATE, DELETE, TRUNCATE ON tb_intents FROM payment_app';
        EXECUTE 'GRANT SELECT, INSERT, UPDATE ON tb_recovery_watermark TO payment_app';
        EXECUTE 'REVOKE DELETE, TRUNCATE ON tb_recovery_watermark FROM payment_app';
    END IF;
END $grants$;
