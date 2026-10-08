-- Least-privilege database roles (deploy hardening, 2026-10).
--
-- Production runs three roles (created by deploy/postgres/10-roles.sh from
-- secret files, never by a migration — passwords do not belong in SQL):
--
--   payment          bootstrap superuser: cluster init and emergencies only.
--   payment_owner    owns the schema; used ONLY to run these migrations at
--                    startup (MIGRATION_DATABASE_URL).
--   payment_app      the runtime role of payment-server and payment-workers
--                    (DATABASE_URL): not the owner, not a superuser, so it can
--                    neither ALTER/DROP tables nor disable the append-only
--                    triggers, and it holds no UPDATE/DELETE on the ledger.
--   payment_backup   pg_read_all_data, for the dump sidecar (no grants here).
--   payment_monitor  pg_monitor, for postgres-exporter (+ the two SELECTs below).
--
-- Dev and test clusters usually have none of these roles: every statement is
-- guarded, so this migration is a no-op there and `sqlx migrate` still runs as
-- whatever single user the developer has.
--
-- Rule for FUTURE migrations (they run as payment_owner): the default
-- privileges set below give payment_app SELECT/INSERT/UPDATE/DELETE on every
-- new table and USAGE/SELECT on every new sequence. A new append-only table
-- must REVOKE UPDATE, DELETE ON <table> FROM payment_app in the migration that
-- creates it (guarded the same way as below).
DO $grants$
DECLARE
    has_app     boolean := EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'payment_app');
    has_owner   boolean := EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'payment_owner');
    has_monitor boolean := EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'payment_monitor');
    rel         regclass;
    missing     text;
BEGIN
    IF NOT has_app THEN
        RAISE NOTICE '0027: role payment_app does not exist — skipping grants (dev/test cluster)';
        RETURN;
    END IF;

    -- Only the owner can grant. An install whose schema still belongs to the
    -- old superuser must be converted first; say so instead of failing on the
    -- first GRANT with a bare "permission denied".
    IF NOT (SELECT rolsuper FROM pg_roles WHERE rolname = current_user) THEN
        SELECT string_agg(format('%I', c.relname), ', ' ORDER BY c.relname) INTO missing
        FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = 'public' AND c.relkind IN ('r', 'p', 'S')
          AND NOT pg_has_role(current_user, c.relowner, 'USAGE');
        IF missing IS NOT NULL THEN
            RAISE EXCEPTION '0027: % does not own: %', current_user, missing
                USING HINT = 'The schema still belongs to the old superuser. Run deploy/upgrade-hardening.sh '
                             '(transfers it to payment_owner), then redeploy.';
        END IF;
    END IF;

    -- The runtime role must never be able to create objects in the schema.
    EXECUTE 'REVOKE CREATE ON SCHEMA public FROM PUBLIC';
    EXECUTE 'GRANT USAGE ON SCHEMA public TO payment_app';

    -- Baseline: plain DML on every table, the sequences behind BIGSERIALs.
    -- (No TRUNCATE, REFERENCES or TRIGGER: those stay with the owner.)
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO payment_app';
    EXECUTE 'GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO payment_app';

    -- Append-only audit trails and the ledger itself: insert + read only.
    -- (voided_transactions, 0023: who voided which key — insert-only too.)
    FOR rel IN
        SELECT c.oid::regclass
        FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = 'public'
          AND c.relkind IN ('r', 'p')
          AND c.relname IN ('entries', 'checkpoints', 'admin_actions',
                            'screening_events', 'biometric_events',
                            'voided_transactions')
    LOOP
        EXECUTE format('REVOKE UPDATE, DELETE, TRUNCATE ON %s FROM payment_app', rel);
    END LOOP;

    -- transactions: insert-only, except the sealer's sealed_seq NULL -> value.
    -- Revoking the table-level UPDATE first also clears any column grants, so
    -- the column grant below is the ONLY update path left.
    IF to_regclass('public.transactions') IS NOT NULL THEN
        EXECUTE 'REVOKE UPDATE, DELETE, TRUNCATE ON public.transactions FROM payment_app';
        EXECUTE 'GRANT UPDATE (sealed_seq) ON public.transactions TO payment_app';
    END IF;

    -- Migration bookkeeping: readable (diagnostics), never writable at runtime.
    IF to_regclass('public._sqlx_migrations') IS NOT NULL THEN
        EXECUTE 'REVOKE INSERT, UPDATE, DELETE, TRUNCATE ON public._sqlx_migrations FROM payment_app';
    END IF;

    -- Reconciliation state (0026): the per-account sums are upserted, never
    -- deleted; the single watermark row is only ever moved forward.
    IF to_regclass('public.reconciled_sums') IS NOT NULL THEN
        EXECUTE 'REVOKE DELETE, TRUNCATE ON public.reconciled_sums FROM payment_app';
    END IF;
    IF to_regclass('public.reconcile_watermark') IS NOT NULL THEN
        EXECUTE 'REVOKE INSERT, DELETE, TRUNCATE ON public.reconcile_watermark FROM payment_app';
    END IF;

    -- Tables and sequences created by LATER migrations (run as payment_owner)
    -- get the same baseline automatically.
    IF has_owner THEN
        IF pg_has_role(current_user, 'payment_owner', 'USAGE') THEN
            EXECUTE 'ALTER DEFAULT PRIVILEGES FOR ROLE payment_owner IN SCHEMA public '
                    'GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO payment_app';
            EXECUTE 'ALTER DEFAULT PRIVILEGES FOR ROLE payment_owner IN SCHEMA public '
                    'GRANT USAGE, SELECT ON SEQUENCES TO payment_app';
        ELSE
            RAISE WARNING '0027: % is not payment_owner (nor a member): default privileges for '
                          'future tables NOT set — run migrations with MIGRATION_DATABASE_URL', current_user;
        END IF;
    END IF;

    -- postgres-exporter's "large deposit" query reads deposit legs only.
    IF has_monitor THEN
        EXECUTE 'GRANT USAGE ON SCHEMA public TO payment_monitor';
        IF to_regclass('public.entries') IS NOT NULL AND to_regclass('public.accounts') IS NOT NULL THEN
            EXECUTE 'GRANT SELECT ON public.entries, public.accounts TO payment_monitor';
        END IF;
    END IF;

    -- A GRANT on a table the current user does not own only WARNs, so verify:
    -- an install whose tables still belong to the old superuser must fail
    -- loudly here (and roll back) instead of booting an app that cannot read.
    -- (has_table_privilege with a list means "any of", hence one call each.)
    SELECT string_agg(format('%I', c.relname), ', ' ORDER BY c.relname) INTO missing
    FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = 'public' AND c.relkind IN ('r', 'p')
      AND (NOT has_table_privilege('payment_app', c.oid, 'SELECT')
           OR (c.relname NOT IN ('_sqlx_migrations', 'reconcile_watermark')
               AND NOT has_table_privilege('payment_app', c.oid, 'INSERT')));
    IF missing IS NOT NULL THEN
        RAISE EXCEPTION '0027: payment_app lacks SELECT/INSERT on: %', missing
            USING HINT = 'The schema is not owned by payment_owner yet. Run deploy/upgrade-hardening.sh, '
                         'then redeploy (the server runs migrations as MIGRATION_DATABASE_URL).';
    END IF;
END
$grants$;
