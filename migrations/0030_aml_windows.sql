-- Per-user AML windows in O(1) (perf pass 2026-10, crates/loadtest/README.md).
--
-- The AML guard (crates/api/src/payments.rs) summed every debit of the user's wallets in the
-- last 24 hours on every post: O(that user's daily debits), so a busy payer got slower as the
-- day went on. It now keeps, per user and currency, the sum and the count of exactly those
-- debits, and moves the window edges instead of re-reading the window:
--
--   day_sum    = SUM(amount_minor) of the user's debit entries in `currency`, created_at >= day_from
--   hour_count = count(*) of the same entries,                                created_at >= hour_from
--
-- Moving an edge adds or subtracts the entries between its old and its new position (a range
-- scan of idx_entries_account_debit_time: each entry is read once as it leaves a window), so
-- the limits stay EXACT — the guard decides on the same set of entries the full sum read — at
-- O(1) amortised per post. A user only gets a row once their day holds 64 debits (api:
-- AML_WINDOW_FROM_DEBITS); until then the guard sums the entries as before, which at that
-- size costs less than keeping a row up to date (measured: crates/loadtest/README.md). New
-- debits are added to existing rows by the trigger below, not by the guard, so every
-- debit counts whichever code path or server version wrote it (a rolling deploy runs old and
-- new servers side by side). Rows change under the user's row lock: the guard takes it, and
-- every post that debits a user wallet goes through the guard. No foreign keys: this is
-- derived state keyed by what the guard and the trigger read from users and accounts, and
-- each post updates its row twice in one transaction, which would re-run the checks — a KEY
-- SHARE on the single hot currencies row per post (the churn 0020 removed from entries).
CREATE TABLE aml_windows (
    user_id     UUID NOT NULL,
    currency    TEXT NOT NULL,
    day_from    TIMESTAMPTZ NOT NULL,
    day_sum     BIGINT NOT NULL CHECK (day_sum >= 0),
    hour_from   TIMESTAMPTZ NOT NULL,
    hour_count  BIGINT NOT NULL CHECK (hour_count >= 0),
    PRIMARY KEY (user_id, currency)
);

-- Statement-level over the transition table, like entries_balanced (0025): storage writes a
-- post's entries in one INSERT, so this runs once per post and touches only the debits of user
-- wallets whose owner already has a window. The created_at conditions keep it exact even for a
-- transaction that started before a window edge.
CREATE OR REPLACE FUNCTION aml_windows_add_debits() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    UPDATE aml_windows w
       SET day_sum = w.day_sum + d.day_sum, hour_count = w.hour_count + d.hour_count
      FROM (SELECT x.user_id, x.currency,
                   COALESCE(SUM(n.amount_minor) FILTER (WHERE n.created_at >= x.day_from), 0)
                       AS day_sum,
                   COUNT(*) FILTER (WHERE n.created_at >= x.hour_from) AS hour_count
              FROM new_entries n
              JOIN accounts a ON a.id = n.account_id AND a.account_type = 'user_wallet'
              JOIN aml_windows x ON x.user_id = a.owner_user_id AND x.currency = a.currency
             WHERE n.direction = 'debit'
             GROUP BY x.user_id, x.currency) d
     WHERE w.user_id = d.user_id AND w.currency = d.currency;
    RETURN NULL;
END $$;

DROP TRIGGER IF EXISTS entries_aml_windows ON entries;
CREATE TRIGGER entries_aml_windows
    AFTER INSERT ON entries
    REFERENCING NEW TABLE AS new_entries
    FOR EACH STATEMENT EXECUTE FUNCTION aml_windows_add_debits();

-- Least privilege (0027's rule): the runtime role reads, creates and moves windows, never
-- deletes them. A no-op without the role (dev/test clusters).
DO $grants$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'payment_app') THEN
        EXECUTE 'GRANT SELECT, INSERT, UPDATE ON aml_windows TO payment_app';
        EXECUTE 'REVOKE DELETE, TRUNCATE ON aml_windows FROM payment_app';
    END IF;
END $grants$;
