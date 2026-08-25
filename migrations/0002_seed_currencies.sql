-- Launch currency. Adding currencies later is data (another INSERT), never a
-- schema change — the "multi-currency-ready from day one" goal.
INSERT INTO currencies (code, exponent, name)
VALUES ('TJS', 2, 'Tajikistani Somoni')
ON CONFLICT (code) DO NOTHING;
