-- Admin flag on users. Deposits (money entering the system from a partner bank)
-- are an administrative/integration action, not something an end user performs
-- on themselves — so the deposit endpoint requires an admin.
--
-- Admins are promoted out-of-band (direct DB / ops tooling), never via the
-- public API. There is intentionally no "make me admin" endpoint.
ALTER TABLE users ADD COLUMN is_admin BOOLEAN NOT NULL DEFAULT false;
