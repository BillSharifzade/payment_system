-- Identity & authentication.

CREATE TABLE users (
    id             UUID PRIMARY KEY,
    phone          TEXT UNIQUE NOT NULL,
    password_hash  TEXT NOT NULL,                 -- Argon2id PHC string; never plaintext
    status         TEXT NOT NULL DEFAULT 'active'
                       CHECK (status IN ('active', 'frozen', 'closed')),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Tie wallets to their owner. NULL for system accounts (settlement, fees, ...).
ALTER TABLE accounts ADD COLUMN owner_user_id UUID REFERENCES users(id);
CREATE INDEX idx_accounts_owner ON accounts (owner_user_id);

-- Refresh tokens: only the SHA-256 hash is stored, so a DB leak yields no usable
-- tokens. Revocable (revoked_at) and expiring (expires_at) — properties a bare
-- JWT cannot offer.
CREATE TABLE refresh_tokens (
    id          UUID PRIMARY KEY,
    user_id     UUID NOT NULL REFERENCES users(id),
    token_hash  TEXT NOT NULL UNIQUE,
    expires_at  TIMESTAMPTZ NOT NULL,
    revoked_at  TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_refresh_tokens_user ON refresh_tokens (user_id);
