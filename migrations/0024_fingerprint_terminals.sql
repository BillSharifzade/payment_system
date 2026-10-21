-- Fingerprint terminal identity, replay detection and attempt lockout (DESIGN.md §20).

-- A merchant's scanner terminal authenticates with its own API key on top of the
-- merchant's session: a stolen merchant token alone cannot drive fingerprint
-- payments. Keys are 32 random bytes shown once at creation; only their SHA-256
-- is stored. Revocation is a timestamp (rows are kept for the audit trail).
CREATE TABLE terminals (
    id                UUID PRIMARY KEY,
    merchant_user_id  UUID NOT NULL REFERENCES users(id),
    label             TEXT NOT NULL,
    key_hash          BYTEA NOT NULL UNIQUE,
    created_by        UUID NOT NULL REFERENCES users(id),
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at        TIMESTAMPTZ,
    last_used_at      TIMESTAMPTZ
);
CREATE INDEX idx_terminals_merchant ON terminals (merchant_user_id, created_at);

-- Which terminal ran each attempt (NULL for attempts logged before terminals existed).
ALTER TABLE biometric_events ADD COLUMN terminal_id UUID REFERENCES terminals(id);

-- A real scanner never produces byte-identical templates twice, so a probe hash
-- seen before is a replayed capture; this index serves that lookup.
CREATE INDEX idx_biometric_events_probe ON biometric_events (probe_hash);

-- 'replayed' is new. The list was only documented in a comment before.
ALTER TABLE biometric_events DROP CONSTRAINT IF EXISTS biometric_events_outcome_check;
ALTER TABLE biometric_events ADD CONSTRAINT biometric_events_outcome_check
    CHECK (outcome IN ('matched', 'no_match', 'ambiguous', 'rejected', 'paid',
                       'matcher_error', 'replayed'));

-- Attempt lockout. A probe reserves a slot (attempts_in_flight + 1, only while
-- failed + in flight < BIOMETRIC_MAX_ATTEMPTS) before the matcher sees it, so
-- concurrent probes can never get more evaluations than the cap allows. The
-- failure that reaches the cap cancels the check in the statement that records
-- it; any other outcome just gives the slot back.
ALTER TABLE checks ADD COLUMN failed_attempts INT NOT NULL DEFAULT 0;
ALTER TABLE checks ADD COLUMN attempts_in_flight INT NOT NULL DEFAULT 0;
