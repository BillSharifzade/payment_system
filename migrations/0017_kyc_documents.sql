-- Track uploaded KYC documents (audit 2026-07-08).
--
-- Uploads previously left no database trace, so (a) a user could fill the
-- document volume at the global rate limit with no per-user quota, and (b)
-- files never referenced by any submission accumulated forever. This table
-- gives uploads an owner and a timestamp: the API enforces a per-user daily
-- upload quota against it, and the retention worker prunes documents that no
-- submission ever cited (the file and the row together).
CREATE TABLE kyc_documents (
    document_ref TEXT PRIMARY KEY,                    -- "<uuid>.<ext>", as returned to the client
    user_id      UUID NOT NULL REFERENCES users (id),
    bytes        BIGINT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The quota check: COUNT per user over a rolling 24h window.
CREATE INDEX idx_kyc_documents_user_time ON kyc_documents (user_id, created_at);
