-- AML / transaction screening (DESIGN.md §11).
--
-- Two pieces:
--  * blocked_users — a sanctions/blocklist. A blocked user can neither send nor
--    receive. Managed by admins.
--  * screening_events — an append-only audit log of every screening decision
--    (allowed or blocked, and which rule fired), for compliance reporting.
CREATE TABLE blocked_users (
    user_id     UUID PRIMARY KEY REFERENCES users(id),
    reason      TEXT NOT NULL,
    blocked_by  UUID REFERENCES users(id),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE screening_events (
    id            UUID PRIMARY KEY,
    user_id       UUID,
    from_account  UUID,
    to_account    UUID,
    amount_minor  BIGINT NOT NULL,
    currency      TEXT NOT NULL,
    decision      TEXT NOT NULL CHECK (decision IN ('allowed', 'blocked')),
    rule          TEXT,             -- which rule blocked it, when decision = 'blocked'
    detail        TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_screening_user_time ON screening_events (user_id, created_at);
