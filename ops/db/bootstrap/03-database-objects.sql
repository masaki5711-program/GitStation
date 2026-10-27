-- forge bootstrap step 3 (ADR-0018 §1 steps 2-6): database privileges,
-- schema forge, the DDL log, the guard functions, the DDL event triggers,
-- cluster_identity() and the bootstrap_log row — one transaction.
-- Run as a superuser, connected to the new database:
--   psql -v ON_ERROR_STOP=1 -d forge -f 03-database-objects.sql
-- Idempotent: a re-run replaces the functions and appends a bootstrap_log
-- row in the same transaction, so its own DDL stays expected.
--
-- Ownership: tables are owned by forge_owner; the guard and DDL-log
-- functions and cluster_identity() stay owned by the superuser running this
-- file, so forge_owner cannot replace them (doctor compares their owner).
-- Event triggers must be owned by a superuser (PostgreSQL).

BEGIN;

SELECT pg_advisory_xact_lock(hashtext('forge.bootstrap.database_objects'));

DO $$
DECLARE
    owner name;
BEGIN
    SELECT pg_catalog.pg_get_userbyid(datdba) INTO owner
    FROM pg_catalog.pg_database WHERE datname = pg_catalog.current_database();
    IF owner <> 'forge_owner' THEN
        RAISE EXCEPTION 'database % is owned by %, not forge_owner (run 02 first)',
            pg_catalog.current_database(), owner;
    END IF;
    EXECUTE format('REVOKE ALL ON DATABASE %I FROM PUBLIC', pg_catalog.current_database());
    EXECUTE format('GRANT CONNECT ON DATABASE %I TO forge_app, forge_migrator, forge_maint',
        pg_catalog.current_database());
    EXECUTE format('ALTER ROLE forge_app IN DATABASE %I SET search_path = forge, public',
        pg_catalog.current_database());
    EXECUTE format('ALTER ROLE forge_maint IN DATABASE %I SET search_path = forge, public',
        pg_catalog.current_database());
    -- _sqlx_migrations lives in public.
    EXECUTE format('ALTER ROLE forge_migrator IN DATABASE %I SET search_path = public, forge',
        pg_catalog.current_database());
END
$$;

REVOKE CREATE ON SCHEMA public FROM PUBLIC;
-- public holds only the migration bookkeeping (_sqlx_migrations, created by
-- forge_owner during forge db migrate); every application object is in
-- schema forge. The read pool must see what is applied (status, check_schema).
ALTER DEFAULT PRIVILEGES FOR ROLE forge_owner IN SCHEMA public
    GRANT SELECT ON TABLES TO forge_app, forge_ro;

CREATE SCHEMA IF NOT EXISTS forge AUTHORIZATION forge_owner;
REVOKE ALL ON SCHEMA forge FROM PUBLIC;
GRANT USAGE ON SCHEMA forge TO forge_app, forge_ro, forge_maint, forge_seq, forge_purge;
-- PostgreSQL requires CREATE on the schema to own functions in it.
GRANT CREATE ON SCHEMA forge TO forge_seq, forge_purge;

-- ── Logs (append-only) ─────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS forge.bootstrap_log (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    bootstrap_version integer NOT NULL,
    txid xid8 NOT NULL DEFAULT pg_catalog.pg_current_xact_id(),
    applied_at timestamptz NOT NULL DEFAULT pg_catalog.now(),
    applied_by text NOT NULL DEFAULT session_user
);

CREATE TABLE IF NOT EXISTS forge.ddl_audit (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    occurred_at timestamptz NOT NULL DEFAULT pg_catalog.now(),
    txid xid8 NOT NULL DEFAULT pg_catalog.pg_current_xact_id(),
    event text NOT NULL,
    session_user_name text NOT NULL,
    application_name text,
    command_tag text NOT NULL,
    object_type text,
    object_identity text,
    parent_identity text
);

CREATE TABLE IF NOT EXISTS forge.ddl_audit_ack (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    ddl_audit_id bigint NOT NULL REFERENCES forge.ddl_audit (id),
    acked_at timestamptz NOT NULL DEFAULT pg_catalog.now(),
    acked_by text NOT NULL DEFAULT session_user,
    note text NOT NULL
);

-- What DDL must not touch without a migration that names it (§3).
-- Migrations append rows for the tables and functions they create.
CREATE TABLE IF NOT EXISTS forge.guarded_objects (
    object_type text NOT NULL,
    object_identity text NOT NULL,
    PRIMARY KEY (object_type, object_identity)
);

ALTER TABLE forge.bootstrap_log OWNER TO forge_owner;
ALTER TABLE forge.ddl_audit OWNER TO forge_owner;
ALTER TABLE forge.ddl_audit_ack OWNER TO forge_owner;
ALTER TABLE forge.guarded_objects OWNER TO forge_owner;

GRANT SELECT ON forge.bootstrap_log, forge.ddl_audit, forge.ddl_audit_ack,
    forge.guarded_objects TO forge_app, forge_ro, forge_maint;
GRANT INSERT (ddl_audit_id, note) ON forge.ddl_audit_ack TO forge_maint;

-- ── Guard: UPDATE / DELETE / TRUNCATE are rejected (SQLSTATE XF001) ────
-- For tables without seq columns only (events / audit_log get their own
-- per-table functions with their migration).
CREATE OR REPLACE FUNCTION forge.reject_log_mutation() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    RAISE EXCEPTION USING
        ERRCODE = 'XF001',
        MESSAGE = format('%s on %I.%I is not allowed (append-only)',
            TG_OP, TG_TABLE_SCHEMA, TG_TABLE_NAME);
END
$$;
REVOKE ALL ON FUNCTION forge.reject_log_mutation() FROM PUBLIC;

DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['bootstrap_log', 'ddl_audit', 'ddl_audit_ack', 'guarded_objects']
    LOOP
        IF NOT EXISTS (
            SELECT 1 FROM pg_catalog.pg_trigger
            WHERE tgrelid = format('forge.%I', t)::regclass
              AND tgname = t || '_append_only'
        ) THEN
            EXECUTE format(
                'CREATE TRIGGER %I BEFORE UPDATE OR DELETE ON forge.%I '
                'FOR EACH ROW EXECUTE FUNCTION forge.reject_log_mutation()',
                t || '_append_only', t);
            EXECUTE format(
                'CREATE TRIGGER %I BEFORE TRUNCATE ON forge.%I '
                'FOR EACH STATEMENT EXECUTE FUNCTION forge.reject_log_mutation()',
                t || '_no_truncate', t);
        END IF;
        -- ALWAYS: also under session_replication_role = replica.
        EXECUTE format('ALTER TABLE forge.%I ENABLE ALWAYS TRIGGER %I', t, t || '_append_only');
        EXECUTE format('ALTER TABLE forge.%I ENABLE ALWAYS TRIGGER %I', t, t || '_no_truncate');
    END LOOP;
END
$$;

-- ── DDL log: facts only; expected / unexpected is decided at read time ─
CREATE OR REPLACE FUNCTION forge.ddl_audit_record() RETURNS event_trigger
    LANGUAGE plpgsql
    SECURITY DEFINER
    SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    r record;
    n integer := 0;
BEGIN
    IF TG_EVENT = 'sql_drop' THEN
        FOR r IN SELECT * FROM pg_catalog.pg_event_trigger_dropped_objects() LOOP
            INSERT INTO forge.ddl_audit (event, session_user_name, application_name,
                command_tag, object_type, object_identity, parent_identity)
            VALUES (TG_EVENT, session_user, current_setting('application_name', true),
                TG_TAG, r.object_type, r.object_identity,
                CASE WHEN r.object_type IN ('trigger', 'table column', 'table constraint',
                                            'rule', 'policy')
                          AND cardinality(r.address_names) >= 2
                     THEN format('%I.%I', r.address_names[1], r.address_names[2])
                END);
            n := n + 1;
        END LOOP;
    ELSE
        FOR r IN SELECT * FROM pg_catalog.pg_event_trigger_ddl_commands() LOOP
            INSERT INTO forge.ddl_audit (event, session_user_name, application_name,
                command_tag, object_type, object_identity, parent_identity)
            VALUES (TG_EVENT, session_user, current_setting('application_name', true),
                r.command_tag, r.object_type, r.object_identity,
                CASE
                    WHEN r.classid = 'pg_catalog.pg_trigger'::regclass THEN
                        (SELECT tgrelid::regclass::text FROM pg_catalog.pg_trigger
                         WHERE oid = r.objid)
                    WHEN r.classid = 'pg_catalog.pg_constraint'::regclass THEN
                        (SELECT conrelid::regclass::text FROM pg_catalog.pg_constraint
                         WHERE oid = r.objid AND conrelid <> 0)
                    WHEN r.classid = 'pg_catalog.pg_class'::regclass AND r.objsubid <> 0 THEN
                        r.objid::regclass::text
                    WHEN r.object_type = 'index' THEN
                        (SELECT indrelid::regclass::text FROM pg_catalog.pg_index
                         WHERE indexrelid = r.objid)
                END);
            n := n + 1;
        END LOOP;
    END IF;
    IF n > 0 THEN
        RAISE WARNING 'forge.ddl_audit: % (% object(s)) recorded', TG_TAG, n;
    END IF;
END
$$;
REVOKE ALL ON FUNCTION forge.ddl_audit_record() FROM PUBLIC;

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_event_trigger
                   WHERE evtname = 'forge_ddl_audit_end') THEN
        CREATE EVENT TRIGGER forge_ddl_audit_end ON ddl_command_end
            EXECUTE FUNCTION forge.ddl_audit_record();
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_event_trigger
                   WHERE evtname = 'forge_ddl_audit_drop') THEN
        CREATE EVENT TRIGGER forge_ddl_audit_drop ON sql_drop
            EXECUTE FUNCTION forge.ddl_audit_record();
    END IF;
END
$$;
ALTER EVENT TRIGGER forge_ddl_audit_end ENABLE ALWAYS;
ALTER EVENT TRIGGER forge_ddl_audit_drop ENABLE ALWAYS;

-- ── Cluster identity (§5): restore / rollback detection ────────────────
CREATE OR REPLACE FUNCTION forge.cluster_identity(
    OUT system_identifier bigint, OUT timeline_id integer)
    LANGUAGE sql
    STABLE
    SECURITY DEFINER
    SET search_path = pg_catalog, pg_temp
AS $$
    SELECT s.system_identifier, c.timeline_id
    FROM pg_catalog.pg_control_system() AS s, pg_catalog.pg_control_checkpoint() AS c
$$;
REVOKE ALL ON FUNCTION forge.cluster_identity() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION forge.cluster_identity() TO forge_app, forge_seq;

-- ── Guarded objects created here ───────────────────────────────────────
INSERT INTO forge.guarded_objects (object_type, object_identity) VALUES
    ('table', 'forge.bootstrap_log'),
    ('table', 'forge.ddl_audit'),
    ('table', 'forge.ddl_audit_ack'),
    ('table', 'forge.guarded_objects'),
    ('function', 'forge.reject_log_mutation()'),
    ('function', 'forge.ddl_audit_record()'),
    ('function', 'forge.cluster_identity()'),
    ('event trigger', 'forge_ddl_audit_end'),
    ('event trigger', 'forge_ddl_audit_drop')
ON CONFLICT DO NOTHING;

-- Same transaction as the DDL above: ddl_audit rows with this txid are the
-- bootstrap's own (§3). Bump the version when this file changes; doctor
-- (db.bootstrap) requires the version its binary was built with.
INSERT INTO forge.bootstrap_log (bootstrap_version) VALUES (1);

COMMIT;
