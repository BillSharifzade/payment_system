-- Shard every hot system account (review 2026-09-20).
--
-- Deposits were already spread over 16 settlement shards (0013), but the fee
-- revenue account and the two FX position accounts were single rows: with a
-- non-zero TRANSFER_FEE_BPS every transfer serialised on one balance row, and
-- every conversion on two. Posting now also applies system-account deltas LAST
-- (see storage), so the hot row is held for one statement plus the commit —
-- sharding removes even that serialisation.
--
-- Id scheme (Uuid::from_u128(base + i), i in 0..16):
--   0x1000  settlement TJS (0013)      0x1100  settlement USD
--   0x2000  fee revenue TJS            0x3000  FX position TJS
--   0x3100  FX position USD
-- The original single rows (…0001..0004) keep their history and stay valid.

INSERT INTO accounts (id, account_type, currency, owner_user_id)
SELECT ('00000000-0000-0000-0000-' || lpad(to_hex(base + g), 12, '0'))::uuid, t, cur, NULL
FROM (VALUES
        (4352, 'system_settlement',   'USD'),   -- 0x1100
        (8192, 'system_fee_revenue',  'TJS'),   -- 0x2000
        (12288, 'system_fx_gain_loss', 'TJS'),  -- 0x3000
        (12544, 'system_fx_gain_loss', 'USD')   -- 0x3100
     ) AS s(base, t, cur)
CROSS JOIN generate_series(0, 15) AS g
ON CONFLICT (id) DO NOTHING;

INSERT INTO balances (account_id, raw_minor, version, min_raw)
SELECT ('00000000-0000-0000-0000-' || lpad(to_hex(base + g), 12, '0'))::uuid, 0, 0, NULL
FROM (VALUES (4352), (8192), (12288), (12544)) AS s(base)
CROSS JOIN generate_series(0, 15) AS g
ON CONFLICT (account_id) DO NOTHING;
