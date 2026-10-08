-- Device binding (DESIGN.md §11): a stolen bearer token alone cannot move money.
--
-- The app keeps a per-install EC P-256 key in the Android Keystore that only a
-- successful device authentication (strong biometric or the screen lock)
-- unlocks. Its public half is registered here — with the account password — and
-- every money move (transfer, FX, app check payment) carries a signature by
-- that key over the canonical payload of the request (crates/api/src/devices.rs),
-- verified against an active device of the caller (DEVICE_BINDING).

CREATE TABLE devices (
    id            UUID PRIMARY KEY,
    user_id       UUID NOT NULL REFERENCES users(id),
    -- SubjectPublicKeyInfo DER of a P-256 key, re-encoded canonically
    -- (uncompressed point) by the server before it is stored.
    public_key    BYTEA NOT NULL CHECK (octet_length(public_key) BETWEEN 64 AND 256),
    label         TEXT NOT NULL CHECK (char_length(label) BETWEEN 1 AND 64),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at  TIMESTAMPTZ,
    revoked_at    TIMESTAMPTZ
);

-- One active row per (user, key): re-registering an active key is idempotent.
-- A shared phone has ONE install key, registered by each user who signs in on
-- it, so the key itself is not unique across users. A revoked key may be
-- registered again (a new row; the revoked one stays for the audit trail).
CREATE UNIQUE INDEX idx_devices_user_key_active
    ON devices (user_id, public_key) WHERE revoked_at IS NULL;
CREATE INDEX idx_devices_user ON devices (user_id, created_at);

-- Who registered or revoked which device, when, from which request.
CREATE TABLE device_events (
    id          UUID PRIMARY KEY,
    user_id     UUID NOT NULL REFERENCES users(id),
    device_id   UUID NOT NULL REFERENCES devices(id),
    action      TEXT NOT NULL CHECK (action IN ('registered', 'revoked')),
    request_id  TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_device_events_user ON device_events (user_id, created_at);

-- The audit trail is append-only, like the other event logs.
CREATE OR REPLACE FUNCTION device_events_reject_rewrite() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION '% on device_events is not allowed: the table is append-only', TG_OP
        USING ERRCODE = 'restrict_violation';
END $$;

CREATE TRIGGER device_events_append_only
    BEFORE UPDATE OR DELETE OR TRUNCATE ON device_events
    FOR EACH STATEMENT EXECUTE FUNCTION device_events_reject_rewrite();

-- Least privilege (0027's rule for new append-only tables): the runtime role may
-- read and insert audit rows, never rewrite them; devices are revoked by a
-- timestamp, never deleted. A no-op without the role (dev/test clusters).
DO $grants$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'payment_app') THEN
        EXECUTE 'GRANT SELECT, INSERT, UPDATE ON devices TO payment_app';
        EXECUTE 'REVOKE DELETE, TRUNCATE ON devices FROM payment_app';
        EXECUTE 'GRANT SELECT, INSERT ON device_events TO payment_app';
        EXECUTE 'REVOKE UPDATE, DELETE, TRUNCATE ON device_events FROM payment_app';
    END IF;
END $grants$;
