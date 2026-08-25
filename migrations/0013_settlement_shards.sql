-- Shard the settlement account across N rows to remove the single hot balance
-- row that every deposit debits. Deposits round-robin across these shards, so
-- concurrent deposits lock different rows and post in parallel (the original
-- single settlement account 00..01 serialised them all on one balance row).
--
-- Shard ids are Uuid::from_u128(0x1000 + i) for i in 0..15, i.e.
-- 00000000-0000-0000-0000-000000001000 .. 000000001000+f. All are TJS
-- system_settlement accounts (debit-normal, may run negative by design), so the
-- system-wide conservation invariant (SUM(raw)=0 per currency) is unaffected.

INSERT INTO accounts (id, account_type, currency, owner_user_id)
SELECT ('00000000-0000-0000-0000-' || lpad(to_hex(4096 + g), 12, '0'))::uuid,
       'system_settlement', 'TJS', NULL
FROM generate_series(0, 15) AS g
ON CONFLICT (id) DO NOTHING;

INSERT INTO balances (account_id, raw_minor, version)
SELECT ('00000000-0000-0000-0000-' || lpad(to_hex(4096 + g), 12, '0'))::uuid, 0, 0
FROM generate_series(0, 15) AS g
ON CONFLICT (account_id) DO NOTHING;
