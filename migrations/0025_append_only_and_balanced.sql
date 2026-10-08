-- Append-only history and balanced transactions, enforced by the database
-- (DESIGN.md §9, §11). Until now both rules held only because the application
-- never broke them; a bug, an ops script or a stolen runtime credential could.
--
-- The runtime role (payment_app) is neither the owner of these tables nor a
-- superuser, so it can neither DISABLE these triggers nor set
-- session_replication_role = replica (which skips ordinary triggers). Only a
-- superuser can bypass them — restore tooling, or a test simulating tampering —
-- and the checkpoint chain and reconciliation exist to catch exactly that.

CREATE OR REPLACE FUNCTION reject_history_rewrite() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION '% on % is not allowed: the table is append-only', TG_OP, TG_TABLE_NAME
        USING ERRCODE = 'restrict_violation',
              HINT = 'Corrections are new (reversing) rows, never edits.';
END $$;

-- Statement-level, so a rejected UPDATE/DELETE costs nothing per row and even a
-- statement that matches no rows is refused (TRUNCATE has no rows at all).
-- voided_transactions (0023) is a record of voided keys and insert-only too.
DO $$
DECLARE
    t TEXT;
BEGIN
    FOREACH t IN ARRAY ARRAY['entries', 'checkpoints', 'admin_actions',
                             'screening_events', 'biometric_events',
                             'voided_transactions'] LOOP
        CONTINUE WHEN to_regclass(t) IS NULL;
        EXECUTE format('DROP TRIGGER IF EXISTS %I ON %I', t || '_append_only', t);
        EXECUTE format(
            'CREATE TRIGGER %I BEFORE UPDATE OR DELETE OR TRUNCATE ON %I
             FOR EACH STATEMENT EXECUTE FUNCTION reject_history_rewrite()',
            t || '_append_only', t);
    END LOOP;
END $$;

DROP TRIGGER IF EXISTS transactions_no_delete ON transactions;
CREATE TRIGGER transactions_no_delete
    BEFORE DELETE OR TRUNCATE ON transactions
    FOR EACH STATEMENT EXECUTE FUNCTION reject_history_rewrite();

-- The only legal UPDATE is the sealer stamping sealed_seq once. Comparing whole
-- rows (rather than listing columns) keeps the rule airtight for any column a
-- later migration adds.
CREATE OR REPLACE FUNCTION transactions_seal_once() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    unsealed transactions;
BEGIN
    IF TG_OP = 'INSERT' THEN
        RAISE EXCEPTION 'transaction % cannot be inserted already sealed', NEW.id
            USING ERRCODE = 'restrict_violation';
    END IF;
    unsealed := NEW;
    unsealed.sealed_seq := NULL;
    IF OLD.sealed_seq IS NULL AND NEW.sealed_seq IS NOT NULL
       AND unsealed IS NOT DISTINCT FROM OLD THEN
        RETURN NEW;
    END IF;
    RAISE EXCEPTION 'transaction % is immutable: only sealed_seq may be set, once', OLD.id
        USING ERRCODE = 'restrict_violation';
END $$;

DROP TRIGGER IF EXISTS transactions_seal_once ON transactions;
CREATE TRIGGER transactions_seal_once
    BEFORE UPDATE ON transactions
    FOR EACH ROW EXECUTE FUNCTION transactions_seal_once();

-- Nor may a row arrive already sealed (that would plant history no checkpoint
-- covers). The WHEN clause is evaluated without calling the function, so an
-- ordinary post pays nothing for it.
DROP TRIGGER IF EXISTS transactions_insert_unsealed ON transactions;
CREATE TRIGGER transactions_insert_unsealed
    BEFORE INSERT ON transactions
    FOR EACH ROW WHEN (NEW.sealed_seq IS NOT NULL)
    EXECUTE FUNCTION transactions_seal_once();

-- Every transaction's entries net to zero per currency. A statement-level check
-- over the transition table: storage writes all of a transaction's entries in
-- one INSERT, so this runs once per post and re-sums only the 2-4 entries of
-- the transactions that statement touched (idx_entries_transaction). Rows can
-- never change afterwards, so checking at insert time is sufficient. A
-- transaction with no entries at all (a voided key) is legal. Entries may not
-- be added to a transaction the sealer has already committed to a checkpoint.
CREATE OR REPLACE FUNCTION entries_must_balance() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    bad RECORD;
BEGIN
    SELECT t.id, t.sealed_seq, e.currency,
           SUM(CASE e.direction WHEN 'credit' THEN e.amount_minor
                                ELSE -e.amount_minor END) AS net
      INTO bad
      FROM (SELECT DISTINCT n.transaction_id FROM new_entries n) n
      JOIN transactions t ON t.id = n.transaction_id
      JOIN entries e ON e.transaction_id = n.transaction_id
     GROUP BY t.id, t.sealed_seq, e.currency
    HAVING t.sealed_seq IS NOT NULL
        OR SUM(CASE e.direction WHEN 'credit' THEN e.amount_minor
                                ELSE -e.amount_minor END) <> 0
     LIMIT 1;
    IF NOT FOUND THEN
        RETURN NULL;
    END IF;
    IF bad.sealed_seq IS NOT NULL THEN
        RAISE EXCEPTION 'transaction % is sealed (sealed_seq %): entries cannot be added',
            bad.id, bad.sealed_seq
            USING ERRCODE = 'restrict_violation';
    END IF;
    RAISE EXCEPTION 'transaction % does not balance: % nets to % minor units',
        bad.id, bad.currency, bad.net
        USING ERRCODE = 'check_violation';
END $$;

DROP TRIGGER IF EXISTS entries_balanced ON entries;
CREATE TRIGGER entries_balanced
    AFTER INSERT ON entries
    REFERENCING NEW TABLE AS new_entries
    FOR EACH STATEMENT EXECUTE FUNCTION entries_must_balance();
