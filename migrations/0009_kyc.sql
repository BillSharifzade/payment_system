-- KYC (Know Your Customer) — DESIGN.md §1.3.
--
-- A user's effective verification level lives on the user row; the audit trail of
-- submissions and admin decisions lives in kyc_submissions. Levels:
--   0 = unverified (cannot transact)
--   1 = basic (identity submitted & approved)
--   2 = full (enhanced due diligence)
--
-- Document storage itself is out of scope here: `document_ref` is a reference
-- (e.g. an object-store key) to material held elsewhere, never the raw document.
ALTER TABLE users ADD COLUMN kyc_level SMALLINT NOT NULL DEFAULT 0;

CREATE TABLE kyc_submissions (
    id                UUID PRIMARY KEY,
    user_id           UUID NOT NULL REFERENCES users(id),
    requested_level   SMALLINT NOT NULL CHECK (requested_level BETWEEN 1 AND 2),
    full_name         TEXT NOT NULL,
    document_type     TEXT NOT NULL,
    document_ref      TEXT NOT NULL,
    status            TEXT NOT NULL DEFAULT 'pending'
                          CHECK (status IN ('pending', 'approved', 'rejected')),
    rejection_reason  TEXT,
    reviewed_by       UUID REFERENCES users(id),
    reviewed_at       TIMESTAMPTZ,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_kyc_submissions_user ON kyc_submissions (user_id);
-- At most one pending submission per user at a time.
CREATE UNIQUE INDEX idx_kyc_one_pending ON kyc_submissions (user_id) WHERE status = 'pending';
