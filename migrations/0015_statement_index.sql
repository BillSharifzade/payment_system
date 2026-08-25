-- Account statements (GET /v1/accounts/{id}/transactions) page over an
-- account's entries newest-first with a (created_at, id) keyset cursor. The
-- account-only index would sort every page request; this one makes each page
-- a bounded index-range scan regardless of history size.
CREATE INDEX idx_entries_account_statement
    ON entries (account_id, created_at DESC, id DESC);
