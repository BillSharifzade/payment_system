-- Biometric (fingerprint) payments — DESIGN.md §20.
--
-- A customer enrols one or more fingers; a merchant creates a *check* (an
-- amount to be collected); the customer pays it by placing a finger on the
-- merchant's scanner. The server identifies the payer (1:N match), verifies
-- funds/KYC/AML and posts the transfer. Nothing here touches the ledger tables:
-- the payment itself is an ordinary double-entry transaction.

-- Enrolled templates. `template` is the vendor/ISO template sealed with
-- AES-256-GCM (nonce || ciphertext, AAD = enrollment id) under
-- BIOMETRIC_TEMPLATE_KEY — the database never holds a usable template.
-- `template_hash` (SHA-256 of format + plaintext) is what the dev "exact"
-- matcher looks up and what prevents the same template being enrolled twice.
-- Rows are never deleted: revocation sets `revoked_at` (audit + legal hold).
CREATE TABLE fingerprint_enrollments (
    id             UUID PRIMARY KEY,
    user_id        UUID NOT NULL REFERENCES users(id),
    finger         SMALLINT NOT NULL CHECK (finger BETWEEN 1 AND 10),  -- ISO 19794-2 position
    format         TEXT NOT NULL,          -- 'iso-19794-2' | 'ansi-378' | 'raw'
    template       BYTEA NOT NULL,         -- sealed, see above
    template_hash  BYTEA NOT NULL,         -- 32 bytes
    quality        SMALLINT,               -- scanner-reported, 0..100, optional
    consent_at     TIMESTAMPTZ NOT NULL,   -- explicit consent captured at enrolment
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at     TIMESTAMPTZ
);
CREATE INDEX idx_fp_enroll_user ON fingerprint_enrollments (user_id);
-- One live template per (user, finger); re-enrolling a finger revokes the old row.
CREATE UNIQUE INDEX idx_fp_enroll_user_finger_live
    ON fingerprint_enrollments (user_id, finger) WHERE revoked_at IS NULL;
-- A live template belongs to exactly one person (and is the exact matcher's key).
CREATE UNIQUE INDEX idx_fp_enroll_hash_live
    ON fingerprint_enrollments (template_hash) WHERE revoked_at IS NULL;

-- Checks: an amount a merchant wants to collect. The id is the client's
-- Idempotency-Key, so a retried create returns the same check. Payment is
-- a state transition open -> paid performed inside the ledger transaction
-- (WHERE status = 'open'), which is what makes a check single-use under
-- concurrency. Expiry is lazy: an open check past `expires_at` is refused
-- and flipped to 'expired' on first touch.
CREATE TABLE checks (
    id                UUID PRIMARY KEY,
    merchant_user_id  UUID NOT NULL REFERENCES users(id),
    merchant_account  UUID NOT NULL REFERENCES accounts(id),
    amount_minor      BIGINT NOT NULL CHECK (amount_minor > 0),
    currency          TEXT NOT NULL REFERENCES currencies(code),
    description       TEXT,
    status            TEXT NOT NULL DEFAULT 'open'
                          CHECK (status IN ('open', 'paid', 'cancelled', 'expired')),
    payer_user_id     UUID REFERENCES users(id),
    payer_account     UUID REFERENCES accounts(id),
    transaction_id    UUID REFERENCES transactions(id),
    method            TEXT,                -- how it was paid: 'fingerprint', ...
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at        TIMESTAMPTZ NOT NULL,
    paid_at           TIMESTAMPTZ,
    cancelled_at      TIMESTAMPTZ
);
CREATE INDEX idx_checks_merchant_time ON checks (merchant_user_id, created_at DESC, id);
CREATE INDEX idx_checks_payer_time ON checks (payer_user_id, created_at DESC)
    WHERE payer_user_id IS NOT NULL;

-- Every identification attempt, successful or not: who ran the terminal,
-- against which check, what the matcher decided. `probe_hash` is the hash of
-- the presented template (never the template itself) so repeated probes can be
-- correlated without storing biometric data.
CREATE TABLE biometric_events (
    id                UUID PRIMARY KEY,
    check_id          UUID REFERENCES checks(id),
    terminal_user_id  UUID NOT NULL REFERENCES users(id),
    matched_user_id   UUID REFERENCES users(id),
    outcome           TEXT NOT NULL,       -- matched | no_match | ambiguous | rejected | paid | matcher_error
    score             DOUBLE PRECISION,
    probe_hash        BYTEA NOT NULL,
    detail            TEXT,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_biometric_events_time ON biometric_events (created_at);
CREATE INDEX idx_biometric_events_terminal ON biometric_events (terminal_user_id, created_at);
CREATE INDEX idx_biometric_events_matched ON biometric_events (matched_user_id, created_at)
    WHERE matched_user_id IS NOT NULL;
