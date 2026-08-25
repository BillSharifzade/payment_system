-- A system fee-revenue account that transfer fees are credited to.
-- Fixed id = 00000000-0000-0000-0000-000000000002 (Uuid::from_u128(2)).
-- Credit-normal; accrues the platform's fee income. TJS for now (one fee account
-- per currency when multi-currency lands).
INSERT INTO accounts (id, account_type, currency, owner_user_id)
VALUES ('00000000-0000-0000-0000-000000000002', 'system_fee_revenue', 'TJS', NULL)
ON CONFLICT (id) DO NOTHING;

INSERT INTO balances (account_id, raw_minor, version)
VALUES ('00000000-0000-0000-0000-000000000002', 0, 0)
ON CONFLICT (account_id) DO NOTHING;
