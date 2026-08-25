-- A well-known system settlement account that deposits draw funds from.
-- Fixed id = 00000000-0000-0000-0000-000000000001 (Uuid::from_u128(1)).
--
-- In production, funding accounts are provisioned per partner-bank integration
-- under admin control; this single seeded account keeps the dev/demo deposit
-- flow working without an admin subsystem yet.
INSERT INTO accounts (id, account_type, currency, owner_user_id)
VALUES ('00000000-0000-0000-0000-000000000001', 'system_settlement', 'TJS', NULL)
ON CONFLICT (id) DO NOTHING;

INSERT INTO balances (account_id, raw_minor, version)
VALUES ('00000000-0000-0000-0000-000000000001', 0, 0)
ON CONFLICT (account_id) DO NOTHING;
