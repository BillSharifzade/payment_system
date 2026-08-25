-- Canonical phone form is now digits-only (E.164 without the '+'): "+992..."
-- and "992..." must be one identity, and a bare '+' does not survive URL query
-- strings. Normalize existing rows; skip any that would collide with an
-- already-normalized duplicate (dev data only — production launches clean).
UPDATE users
SET phone = substr(phone, 2)
WHERE phone LIKE '+%'
  AND NOT EXISTS (SELECT 1 FROM users u2 WHERE u2.phone = substr(users.phone, 2));
