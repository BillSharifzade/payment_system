-- API-level idempotency. Every state-changing endpoint requires an
-- `Idempotency-Key` (a UUID), which we also use as the transaction id — so the
-- ledger's transaction-id primary key already guarantees the *money* is never
-- moved twice. This table additionally lets us:
--   * detect a key reused with a *different* request body (a client bug) and
--     reject it with 409, and
--   * return the identical response on a legitimate retry.
--
-- The fingerprint is the canonical request string; mismatch on the same key
-- means the key was reused for a different operation.
CREATE TABLE idempotency_keys (
    key              UUID PRIMARY KEY,
    fingerprint      TEXT NOT NULL,
    response_status  INT NOT NULL,
    response_body    JSONB NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
