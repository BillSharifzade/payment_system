-- Voided keys and dual-control deposits (hardening pass 2026-10).

-- A client that gives up on an unsettled payment voids its key instead of
-- guessing from the statement: the void claims the id in `transactions` (a row
-- with NO entries, insert-only like every other) so the ledger's primary key
-- guarantees the key can never post afterwards, and records who voided it.
-- Sealed by the checkpoint chain like any transaction, so a void is as
-- tamper-evident as a payment.
CREATE TABLE voided_transactions (
    id          UUID PRIMARY KEY REFERENCES transactions(id),
    voided_by   UUID NOT NULL REFERENCES users(id),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Money creation needs two people: one admin requests a deposit, a different
-- admin (who does not own the wallet) approves it. `id` is the request's
-- Idempotency-Key and becomes the ledger transaction id on approval. The
-- pending -> posted flip runs inside the posting transaction's guard
-- (WHERE status = 'pending_approval'), which makes an approval single-use under
-- concurrency exactly like a check payment. Every action is in admin_actions.
CREATE TABLE deposit_requests (
    id            UUID PRIMARY KEY,
    user_account  UUID NOT NULL REFERENCES accounts(id),
    amount_minor  BIGINT NOT NULL CHECK (amount_minor > 0),
    currency      TEXT NOT NULL REFERENCES currencies(code),
    status        TEXT NOT NULL DEFAULT 'pending_approval'
                      CHECK (status IN ('pending_approval', 'posted', 'rejected')),
    requested_by  UUID NOT NULL REFERENCES users(id),
    requested_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    decided_by    UUID REFERENCES users(id),
    decided_at    TIMESTAMPTZ,
    reason        TEXT,
    CHECK ((status = 'pending_approval') = (decided_by IS NULL))
);
-- The approval queue pages by keyset per status.
CREATE INDEX idx_deposit_requests_status ON deposit_requests (status, requested_at, id);
CREATE INDEX idx_deposit_requests_account ON deposit_requests (user_account);
