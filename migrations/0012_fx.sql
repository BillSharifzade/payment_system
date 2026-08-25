-- FX / multi-currency support (DESIGN.md §4.2).
--
-- A currency conversion is a single transaction with TWO balanced legs — one per
-- currency — so double-entry holds independently in each. The platform's FX
-- position accumulates in per-currency system accounts; rounding never creates
-- or destroys money (the floored remainder stays in the platform's position).

-- Second launch-adjacent currency so FX is exercisable.
INSERT INTO currencies (code, exponent, name)
VALUES ('USD', 2, 'US Dollar')
ON CONFLICT (code) DO NOTHING;

-- One FX-position account per currency.
--   TJS fx position = Uuid::from_u128(3), USD fx position = Uuid::from_u128(4).
INSERT INTO accounts (id, account_type, currency, owner_user_id) VALUES
    ('00000000-0000-0000-0000-000000000003', 'system_fx_gain_loss', 'TJS', NULL),
    ('00000000-0000-0000-0000-000000000004', 'system_fx_gain_loss', 'USD', NULL)
ON CONFLICT (id) DO NOTHING;
INSERT INTO balances (account_id, raw_minor, version) VALUES
    ('00000000-0000-0000-0000-000000000003', 0, 0),
    ('00000000-0000-0000-0000-000000000004', 0, 0)
ON CONFLICT (account_id) DO NOTHING;

-- Admin-managed conversion rates, expressed as an exact integer ratio of
-- quote-minor per base-minor: amount_quote = amount_base * rate_num / rate_den.
-- Clients never set their own rate.
CREATE TABLE fx_rates (
    base_currency   TEXT NOT NULL REFERENCES currencies(code),
    quote_currency  TEXT NOT NULL REFERENCES currencies(code),
    rate_num        BIGINT NOT NULL CHECK (rate_num > 0),
    rate_den        BIGINT NOT NULL CHECK (rate_den > 0),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (base_currency, quote_currency)
);
