-- Transactional outbox (DESIGN.md §5.4).
--
-- An event row is written in the SAME database transaction as the ledger
-- entries it describes. A separate relay then publishes unsent rows. This makes
-- "the money moved" and "an event was emitted" a single atomic fact — no dual
-- write, so no lost or phantom events even if the process crashes mid-publish.
CREATE TABLE outbox (
    id            UUID PRIMARY KEY,
    aggregate_id  UUID NOT NULL,        -- the transaction id this event is about
    event_type    TEXT NOT NULL,        -- e.g. 'transaction.posted'
    payload       JSONB NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    sent_at       TIMESTAMPTZ           -- NULL until the relay has published it
);

-- A partial index so the relay's "find unsent" scan stays cheap as the table grows.
CREATE INDEX idx_outbox_unsent ON outbox (created_at) WHERE sent_at IS NULL;
