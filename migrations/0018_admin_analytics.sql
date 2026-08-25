-- Admin analytics + browsable user list (console overhaul 2026-07-09).
--
-- The dashboard's time-bucketed queries and the users screen's list/search all
-- run against bounded windows or prefixes; these indexes keep every one of
-- them on an index path instead of a sequential scan, no matter how large the
-- ledger grows.

-- Daily volume (SUM over a 30-day window of entries) and daily transaction
-- counts scan by time.
CREATE INDEX idx_entries_created ON entries (created_at);
CREATE INDEX idx_transactions_created ON transactions (created_at);

-- The users list pages newest-first by keyset, and the search box does a
-- phone-prefix lookup (text_pattern_ops makes LIKE 'prefix%' indexable under
-- any collation).
CREATE INDEX idx_users_created ON users (created_at DESC, id DESC);
CREATE INDEX idx_users_phone_prefix ON users (phone text_pattern_ops);

-- "AML blocks in the last 30 days" — partial, so it only ever holds the rare
-- blocked rows, not the allowed firehose.
CREATE INDEX idx_screening_blocked_time ON screening_events (created_at)
    WHERE decision = 'blocked';

-- KYC review throughput buckets by review time.
CREATE INDEX idx_kyc_reviewed ON kyc_submissions (reviewed_at)
    WHERE reviewed_at IS NOT NULL;
