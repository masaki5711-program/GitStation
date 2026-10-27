-- forge bootstrap step 3 (ADR-0018 §1 steps 2-6): database privileges,
-- schemas forge and forge_audit, the DDL log, the guard functions, the DDL
-- event triggers, cluster_identity() and the bootstrap_log row — one
-- transaction.
-- Run as a superuser, connected to the new database:
--   psql -d forge -f 03-database-objects.sql
-- Idempotent and self-healing: a re-run recreates the guard triggers and
-- event triggers and replaces the functions, and appends a bootstrap_log
-- row in the same transaction, so its own DDL stays expected.
--
-- Ownership (ADR-0018 amendment of 2026-10-27):
-- - schema forge_audit, the log tables and the guard functions belong to
--   the role forge_audit (NOLOGIN, no members). Nothing forge_owner or a
--   login can become owns anything the superuser-owned event triggers or
--   this file write to or call, so no lower role can attach code
--   (triggers, defaults, overloads) that would run with higher rights, and
--   the schema owner forge_owner cannot drop them (it does not own the
--   schema);
-- - cluster_identity() and the event triggers belong to the superuser
--   running this file (pg_control_* and event triggers need one);
-- - schema forge belongs to forge_owner (migrations create objects there,
--   and the migration bookkeeping forge._sqlx_migrations lives there);
-- - the database and schema public belong to the superuser: forge_owner
--   cannot create schemas, write public, install extensions or set session
--   defaults for the database, so it cannot plant code that a superuser
--   session would resolve.
-- The session resolves names in pg_catalog only and runs without timeouts
-- or role switches a database default could impose.
\set ON_ERROR_STOP on

BEGIN;

-- The database owner can set session defaults for this database (role,
-- search_path, read-only): undo them for this session first.
SET LOCAL ROLE NONE;
SET LOCAL search_path = pg_catalog, pg_temp;
SET LOCAL statement_timeout = 0;
SET LOCAL lock_timeout = 0;

-- Every grant to anyone but the owner on the objects this file manages is
-- revoked first (whatever the grantee, with CASCADE for grants made through
-- a grant option); the statements below grant the shipped set again. A
-- grantee outside the forge roles is named in a warning.
DO $$
DECLARE
    o record;
    g record;
BEGIN
    FOR o IN
        SELECT 'DATABASE' AS kind, pg_catalog.format('%I', d.datname) AS name,
               coalesce(d.datacl, pg_catalog.acldefault('d', d.datdba)) AS acl, d.datdba AS owner
        FROM pg_catalog.pg_database d WHERE d.datname = pg_catalog.current_database()
        UNION ALL
        SELECT 'SCHEMA', pg_catalog.format('%I', n.nspname),
               coalesce(n.nspacl, pg_catalog.acldefault('n', n.nspowner)), n.nspowner
        FROM pg_catalog.pg_namespace n WHERE n.nspname IN ('public', 'forge', 'forge_audit')
        UNION ALL
        SELECT 'TABLE', c.oid::pg_catalog.regclass::text,
               coalesce(c.relacl, pg_catalog.acldefault('r', c.relowner)), c.relowner
        FROM pg_catalog.pg_class c
        WHERE c.oid IN (pg_catalog.to_regclass('forge_audit.bootstrap_log'),
                        pg_catalog.to_regclass('forge_audit.ddl_audit'),
                        pg_catalog.to_regclass('forge_audit.ddl_audit_ack'),
                        pg_catalog.to_regclass('forge_audit.guarded_objects'),
                        pg_catalog.to_regclass('forge._sqlx_migrations'))
    LOOP
        FOR g IN
            SELECT DISTINCT a.grantee, r.rolname
            FROM pg_catalog.aclexplode(o.acl) a
            LEFT JOIN pg_catalog.pg_roles r ON r.oid = a.grantee
            WHERE a.grantee <> o.owner
            UNION
            -- Column grants (a table-level REVOKE ALL removes them too).
            SELECT DISTINCT a.grantee, r.rolname
            FROM pg_catalog.pg_attribute t, pg_catalog.aclexplode(t.attacl) a
            LEFT JOIN pg_catalog.pg_roles r ON r.oid = a.grantee
            WHERE o.kind = 'TABLE' AND t.attrelid = o.name::pg_catalog.regclass
              AND t.attacl IS NOT NULL AND a.grantee <> o.owner
        LOOP
            IF g.grantee <> 0 AND g.rolname NOT LIKE 'forge\_%' THEN
                RAISE WARNING 'forge bootstrap: revoking privileges of % on % %',
                    g.rolname, o.kind, o.name;
            END IF;
            EXECUTE pg_catalog.format('REVOKE ALL ON %s %s FROM %s CASCADE', o.kind, o.name,
                CASE WHEN g.grantee = 0 THEN 'PUBLIC' ELSE pg_catalog.format('%I', g.rolname) END);
        END LOOP;
    END LOOP;
END
$$;


SELECT pg_catalog.pg_advisory_xact_lock(pg_catalog.hashtext('forge.bootstrap.database_objects'));

DO $$
DECLARE
    owner name;
    schema_owner name;
    setting record;
BEGIN
    SELECT pg_catalog.pg_get_userbyid(nspowner) INTO schema_owner
    FROM pg_catalog.pg_namespace WHERE nspname = 'forge_audit';
    IF schema_owner IS NOT NULL AND schema_owner <> 'forge_audit' THEN
        RAISE EXCEPTION 'schema forge_audit exists and is owned by %, not forge_audit',
            schema_owner;
    END IF;
    -- (SET LOCAL ROLE NONE above made current_user the login.)
    IF NOT (SELECT rolsuper FROM pg_catalog.pg_roles WHERE rolname = current_user) THEN
        RAISE EXCEPTION 'run 03 as a superuser (current_user %)', current_user;
    END IF;
    -- A database from bootstrap version 2 kept the migration bookkeeping in
    -- public; an empty forge._sqlx_migrations next to it would make every
    -- migration pending again. Move it first (ops/runbooks/db-bootstrap.md).
    IF pg_catalog.to_regclass('public._sqlx_migrations') IS NOT NULL THEN
        RAISE EXCEPTION 'public._sqlx_migrations exists: move it with ALTER TABLE public._sqlx_migrations SET SCHEMA forge (runbook db-bootstrap) before running 03';
    END IF;
    SELECT r.rolname INTO owner
    FROM pg_catalog.pg_database d JOIN pg_catalog.pg_roles r ON r.oid = d.datdba
    WHERE d.datname = pg_catalog.current_database() AND r.rolsuper;
    IF owner IS NULL THEN
        RAISE EXCEPTION 'database % must be owned by a superuser (create it with 02)',
            pg_catalog.current_database();
    END IF;
    -- Only the login search_paths below are set for this database, and no
    -- forge role carries a role-wide setting: every other setting is
    -- removed, each one named in a warning (an operator's setting such as
    -- log_min_duration_statement is lost too — the runbook says so).
    FOR setting IN
        SELECT s.setrole, r.rolname, s.setdatabase, s.setconfig
        FROM pg_catalog.pg_db_role_setting s
        LEFT JOIN pg_catalog.pg_roles r ON r.oid = s.setrole
        WHERE s.setdatabase = (SELECT oid FROM pg_catalog.pg_database
                               WHERE datname = pg_catalog.current_database())
           OR (s.setdatabase = 0 AND r.rolname = ANY (ARRAY['forge_owner', 'forge_migrator',
               'forge_seq', 'forge_purge', 'forge_app', 'forge_ro', 'forge_maint', 'forge_audit']))
    EXCEPT
    -- The login search_paths this file sets anyway (no warning on a re-run).
    SELECT s.setrole, r.rolname, s.setdatabase, s.setconfig
        FROM pg_catalog.pg_db_role_setting s
        JOIN pg_catalog.pg_roles r ON r.oid = s.setrole
        WHERE s.setdatabase = (SELECT oid FROM pg_catalog.pg_database
                               WHERE datname = pg_catalog.current_database())
          AND (r.rolname, s.setconfig) IN (
              ('forge_app', ARRAY['search_path=forge, public']),
              ('forge_maint', ARRAY['search_path=pg_catalog, pg_temp']),
              ('forge_migrator', ARRAY['search_path=forge']))
    LOOP
        RAISE WARNING 'forge bootstrap: removing session defaults % (role %, %)',
            setting.setconfig, coalesce(setting.rolname, 'all'),
            CASE WHEN setting.setdatabase = 0 THEN 'every database' ELSE 'this database' END;
        IF setting.setdatabase = 0 THEN
            EXECUTE format('ALTER ROLE %I RESET ALL', setting.rolname);
        ELSIF setting.setrole = 0 THEN
            EXECUTE format('ALTER DATABASE %I RESET ALL', pg_catalog.current_database());
        ELSE
            EXECUTE format('ALTER ROLE %I IN DATABASE %I RESET ALL', setting.rolname,
                pg_catalog.current_database());
        END IF;
    END LOOP;
    -- Every ACL below is rebuilt from nothing on each run: a re-run removes
    -- grants added since (doctor's guard=privileges compares exactly).
    EXECUTE format('REVOKE ALL ON DATABASE %I FROM PUBLIC, forge_owner, forge_migrator, forge_seq, forge_purge, forge_app, forge_ro, forge_maint, forge_audit',
        pg_catalog.current_database());
    EXECUTE format('GRANT CONNECT ON DATABASE %I TO forge_app, forge_migrator, forge_maint',
        pg_catalog.current_database());
    EXECUTE format('ALTER ROLE forge_app IN DATABASE %I SET search_path = forge, public',
        pg_catalog.current_database());
    -- Maintenance commands qualify every name; nothing writable is searched.
    EXECUTE format('ALTER ROLE forge_maint IN DATABASE %I SET search_path = pg_catalog, pg_temp',
        pg_catalog.current_database());
    -- Migrations create (unqualified) in forge, where _sqlx_migrations is.
    EXECUTE format('ALTER ROLE forge_migrator IN DATABASE %I SET search_path = forge',
        pg_catalog.current_database());
END
$$;

-- public belongs to the database owner (this superuser) and holds nothing.
REVOKE ALL ON SCHEMA public FROM PUBLIC, forge_owner, forge_migrator, forge_seq, forge_purge, forge_app, forge_ro, forge_maint, forge_audit;
GRANT USAGE ON SCHEMA public TO PUBLIC;

CREATE SCHEMA IF NOT EXISTS forge AUTHORIZATION forge_owner;
ALTER SCHEMA forge OWNER TO forge_owner;
REVOKE ALL ON SCHEMA forge FROM PUBLIC, forge_migrator, forge_seq, forge_purge, forge_app, forge_ro, forge_maint, forge_audit;
GRANT USAGE ON SCHEMA forge TO forge_app, forge_ro, forge_maint, forge_seq, forge_purge;
-- PostgreSQL requires CREATE on the schema to own functions in it.
GRANT CREATE ON SCHEMA forge TO forge_seq, forge_purge;

-- The migration bookkeeping, created here so that it exists before the
-- first migration (the read pool reads it; its DDL is the bootstrap's).
-- The definition is sqlx-postgres 0.8's (CREATE TABLE IF NOT EXISTS there
-- then does nothing); a test runs real migrations against it.
CREATE TABLE IF NOT EXISTS forge._sqlx_migrations (
    version BIGINT PRIMARY KEY,
    description TEXT NOT NULL,
    installed_on TIMESTAMPTZ NOT NULL DEFAULT pg_catalog.now(),
    success BOOLEAN NOT NULL,
    checksum BYTEA NOT NULL,
    execution_time BIGINT NOT NULL
);
ALTER TABLE forge._sqlx_migrations OWNER TO forge_owner;
REVOKE ALL ON forge._sqlx_migrations FROM PUBLIC, forge_migrator, forge_seq, forge_purge, forge_app, forge_ro, forge_maint, forge_audit;
GRANT SELECT ON forge._sqlx_migrations TO forge_app, forge_ro;

CREATE SCHEMA IF NOT EXISTS forge_audit AUTHORIZATION forge_audit;
REVOKE ALL ON SCHEMA forge_audit FROM PUBLIC, forge_owner, forge_migrator, forge_seq, forge_purge, forge_app, forge_ro, forge_maint;
GRANT USAGE ON SCHEMA forge_audit TO forge_app, forge_ro, forge_maint, forge_owner, forge_seq,
    forge_purge;

-- Functions created later by the migration roles are not executable by
-- PUBLIC unless a migration grants it (each SECURITY DEFINER function also
-- revokes it in its own file — the gate checks that).
-- Any other default privilege of these roles (a role may change its own)
-- is removed first.
DO $$
DECLARE
    d record;
BEGIN
    FOR d IN
        SELECT r.rolname, n.nspname, a.grantee, g.rolname AS grantee_name,
               CASE dacl.defaclobjtype WHEN 'r' THEN 'TABLES' WHEN 'S' THEN 'SEQUENCES'
                    WHEN 'f' THEN 'FUNCTIONS' WHEN 'T' THEN 'TYPES' WHEN 'n' THEN 'SCHEMAS' END
                   AS kind
        FROM pg_catalog.pg_default_acl dacl
        JOIN pg_catalog.pg_roles r ON r.oid = dacl.defaclrole
        LEFT JOIN pg_catalog.pg_namespace n ON n.oid = dacl.defaclnamespace,
        pg_catalog.aclexplode(dacl.defaclacl) a
        LEFT JOIN pg_catalog.pg_roles g ON g.oid = a.grantee
        WHERE r.rolname IN ('forge_owner', 'forge_seq', 'forge_purge')
          AND (a.grantee <> dacl.defaclrole OR dacl.defaclnamespace <> 0)
    LOOP
        RAISE WARNING 'forge bootstrap: removing default privilege of % on % for %',
            d.rolname, d.kind, coalesce(d.grantee_name, 'PUBLIC');
        EXECUTE format('ALTER DEFAULT PRIVILEGES FOR ROLE %I %s REVOKE ALL ON %s FROM %s',
            d.rolname,
            CASE WHEN d.nspname IS NULL THEN '' ELSE format('IN SCHEMA %I', d.nspname) END,
            d.kind,
            CASE WHEN d.grantee = 0 THEN 'PUBLIC' ELSE format('%I', d.grantee_name) END);
    END LOOP;
END
$$;
ALTER DEFAULT PRIVILEGES FOR ROLE forge_owner, forge_seq, forge_purge
    REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;

-- ── Logs (append-only, owned by forge_audit) ───────────────────────────
CREATE TABLE IF NOT EXISTS forge_audit.bootstrap_log (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    bootstrap_version integer NOT NULL,
    txid xid8 NOT NULL DEFAULT pg_catalog.pg_current_xact_id(),
    applied_at timestamptz NOT NULL DEFAULT pg_catalog.now(),
    applied_by text NOT NULL DEFAULT session_user
);

CREATE TABLE IF NOT EXISTS forge_audit.ddl_audit (
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

CREATE TABLE IF NOT EXISTS forge_audit.ddl_audit_ack (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    ddl_audit_id bigint NOT NULL REFERENCES forge_audit.ddl_audit (id),
    acked_at timestamptz NOT NULL DEFAULT pg_catalog.now(),
    acked_by text NOT NULL DEFAULT session_user,
    note text NOT NULL
);

-- What DDL must not touch without a migration that names it (§3).
-- Migrations (forge_owner) append rows for the objects they create.
CREATE TABLE IF NOT EXISTS forge_audit.guarded_objects (
    object_type text NOT NULL,
    object_identity text NOT NULL,
    PRIMARY KEY (object_type, object_identity)
);

ALTER TABLE forge_audit.bootstrap_log OWNER TO forge_audit;
ALTER TABLE forge_audit.ddl_audit OWNER TO forge_audit;
ALTER TABLE forge_audit.ddl_audit_ack OWNER TO forge_audit;
ALTER TABLE forge_audit.guarded_objects OWNER TO forge_audit;

REVOKE ALL ON forge_audit.bootstrap_log, forge_audit.ddl_audit, forge_audit.ddl_audit_ack,
    forge_audit.guarded_objects FROM PUBLIC, forge_owner, forge_migrator, forge_seq, forge_purge, forge_app, forge_ro, forge_maint;
GRANT SELECT ON forge_audit.bootstrap_log, forge_audit.ddl_audit, forge_audit.ddl_audit_ack,
    forge_audit.guarded_objects TO forge_app, forge_ro, forge_maint, forge_owner;
GRANT INSERT (ddl_audit_id, note) ON forge_audit.ddl_audit_ack TO forge_maint;
GRANT INSERT (object_type, object_identity) ON forge_audit.guarded_objects TO forge_owner;

-- ── Guard: UPDATE / DELETE / TRUNCATE are rejected (SQLSTATE XF001) ────
-- For tables without seq columns only (events / audit_log get their own
-- per-table functions with their migration). Attached by this file only.
CREATE OR REPLACE FUNCTION forge_audit.reject_log_mutation() RETURNS trigger
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
ALTER FUNCTION forge_audit.reject_log_mutation() OWNER TO forge_audit;
REVOKE ALL ON FUNCTION forge_audit.reject_log_mutation() FROM PUBLIC, forge_owner, forge_migrator, forge_seq,
    forge_purge, forge_app, forge_ro, forge_maint;

-- Recreated on every run, so a re-run also repairs a replaced trigger.
DO $$
DECLARE
    t text;
    extra record;
    rule record;
BEGIN
    FOREACH t IN ARRAY ARRAY['bootstrap_log', 'ddl_audit', 'ddl_audit_ack', 'guarded_objects']
    LOOP
        -- No rules or row security on a log table.
        FOR rule IN
            SELECT rw.rulename FROM pg_catalog.pg_rewrite rw
            WHERE rw.ev_class = format('forge_audit.%I', t)::regclass
        LOOP
            RAISE WARNING 'forge bootstrap: dropping unexpected rule % on forge_audit.%',
                rule.rulename, t;
            EXECUTE format('DROP RULE %I ON forge_audit.%I', rule.rulename, t);
        END LOOP;
        EXECUTE format('ALTER TABLE forge_audit.%I DISABLE ROW LEVEL SECURITY, NO FORCE ROW LEVEL SECURITY', t);
        -- Only the shipped guard triggers may exist on a log table.
        FOR extra IN
            SELECT tg.tgname FROM pg_catalog.pg_trigger tg
            WHERE tg.tgrelid = format('forge_audit.%I', t)::regclass AND NOT tg.tgisinternal
              AND tg.tgname NOT IN (t || '_append_only', t || '_no_truncate')
        LOOP
            RAISE WARNING 'forge bootstrap: dropping unexpected trigger % on forge_audit.%',
                extra.tgname, t;
            EXECUTE format('DROP TRIGGER %I ON forge_audit.%I', extra.tgname, t);
        END LOOP;
        EXECUTE format('DROP TRIGGER IF EXISTS %I ON forge_audit.%I', t || '_append_only', t);
        EXECUTE format('DROP TRIGGER IF EXISTS %I ON forge_audit.%I', t || '_no_truncate', t);
        EXECUTE format(
            'CREATE TRIGGER %I BEFORE UPDATE OR DELETE ON forge_audit.%I '
            'FOR EACH ROW EXECUTE FUNCTION forge_audit.reject_log_mutation()',
            t || '_append_only', t);
        EXECUTE format(
            'CREATE TRIGGER %I BEFORE TRUNCATE ON forge_audit.%I '
            'FOR EACH STATEMENT EXECUTE FUNCTION forge_audit.reject_log_mutation()',
            t || '_no_truncate', t);
        -- ALWAYS: also under session_replication_role = replica.
        EXECUTE format('ALTER TABLE forge_audit.%I ENABLE ALWAYS TRIGGER %I', t, t || '_append_only');
        EXECUTE format('ALTER TABLE forge_audit.%I ENABLE ALWAYS TRIGGER %I', t, t || '_no_truncate');
    END LOOP;
END
$$;

-- ── DDL log: facts only; expected / unexpected is decided at read time ─
-- SECURITY DEFINER as forge_audit (the table owner). Every object is
-- qualified, so the search_path holds no schema a lower role can write.
CREATE OR REPLACE FUNCTION forge_audit.ddl_audit_record() RETURNS event_trigger
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
            INSERT INTO forge_audit.ddl_audit (event, session_user_name, application_name,
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
            INSERT INTO forge_audit.ddl_audit (event, session_user_name, application_name,
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
        RAISE WARNING 'forge_audit.ddl_audit: % (% object(s)) recorded', TG_TAG, n;
    END IF;
END
$$;
ALTER FUNCTION forge_audit.ddl_audit_record() OWNER TO forge_audit;
REVOKE ALL ON FUNCTION forge_audit.ddl_audit_record() FROM PUBLIC, forge_owner, forge_migrator, forge_seq,
    forge_purge, forge_app, forge_ro, forge_maint;

-- Recreated on every run (repairs a re-pointed or disabled trigger); any
-- other event trigger calling a forge_audit function is removed too.
DO $$
DECLARE
    e record;
BEGIN
    FOR e IN
        SELECT evt.evtname FROM pg_catalog.pg_event_trigger evt
        JOIN pg_catalog.pg_proc fn ON fn.oid = evt.evtfoid
        JOIN pg_catalog.pg_namespace ns ON ns.oid = fn.pronamespace
        WHERE ns.nspname = 'forge_audit'
          AND evt.evtname NOT IN ('forge_ddl_audit_end', 'forge_ddl_audit_drop')
    LOOP
        RAISE WARNING 'forge bootstrap: dropping unexpected event trigger %', e.evtname;
        EXECUTE format('DROP EVENT TRIGGER %I', e.evtname);
    END LOOP;
END
$$;
DROP EVENT TRIGGER IF EXISTS forge_ddl_audit_end;
DROP EVENT TRIGGER IF EXISTS forge_ddl_audit_drop;
CREATE EVENT TRIGGER forge_ddl_audit_end ON ddl_command_end
    EXECUTE FUNCTION forge_audit.ddl_audit_record();
CREATE EVENT TRIGGER forge_ddl_audit_drop ON sql_drop
    EXECUTE FUNCTION forge_audit.ddl_audit_record();
ALTER EVENT TRIGGER forge_ddl_audit_end ENABLE ALWAYS;
ALTER EVENT TRIGGER forge_ddl_audit_drop ENABLE ALWAYS;

-- ── Cluster identity (§5): restore / rollback detection ────────────────
-- Owned by the superuser (pg_control_* needs one); it writes nothing.
CREATE OR REPLACE FUNCTION forge_audit.cluster_identity(
    OUT system_identifier bigint, OUT timeline_id integer)
    LANGUAGE sql
    STABLE
    SECURITY DEFINER
    SET search_path = pg_catalog, pg_temp
AS $$
    SELECT s.system_identifier, c.timeline_id
    FROM pg_catalog.pg_control_system() AS s, pg_catalog.pg_control_checkpoint() AS c
$$;
REVOKE ALL ON FUNCTION forge_audit.cluster_identity() FROM PUBLIC, forge_owner, forge_migrator, forge_seq,
    forge_purge, forge_app, forge_ro, forge_maint;
GRANT EXECUTE ON FUNCTION forge_audit.cluster_identity() TO forge_app, forge_seq;

-- ── DDL status (§3, decided at read time) ──────────────────────────────
-- Owned by forge_audit (forge_owner cannot replace or drop it; doctor
-- compares its body) and SECURITY INVOKER: the caller needs SELECT on the
-- DDL log and on forge.audit_log (txid, action, details), which exists only
-- after the first migration. A row is expected when its transaction is a
-- bootstrap run, or a migration (a db.migrate row in forge.audit_log,
-- forge_migrator, application_name forge-migrate) whose guarded_changes
-- name the row's object if it is guarded: a trigger, rule, policy or
-- constraint by its own name (forge.<table>.<name>), a function without
-- its argument list, other objects by name or by their parent table.
-- GRANT / REVOKE rows carry no object (PostgreSQL); doctor's privilege
-- checks cover them, so inside a migration they count as expected.
CREATE OR REPLACE FUNCTION forge_audit.ddl_audit_status()
    RETURNS TABLE (id bigint, occurred_at timestamptz, txid xid8, session_user_name text,
                   application_name text, command_tag text, object_type text,
                   object_identity text, parent_identity text, expected boolean,
                   acknowledged boolean)
    LANGUAGE plpgsql
    STABLE
    SET search_path = pg_catalog, pg_temp
AS $$
#variable_conflict use_column
BEGIN
    RETURN QUERY
    SELECT d.id, d.occurred_at, d.txid, d.session_user_name, d.application_name,
           d.command_tag, d.object_type, d.object_identity, d.parent_identity,
           (EXISTS (SELECT 1 FROM forge_audit.bootstrap_log AS b WHERE b.txid = d.txid)
            OR (m.present
                AND d.session_user_name = 'forge_migrator'
                AND d.application_name = 'forge-migrate'
                AND (NOT g.guarded OR m.changes ?| n.names))),
           EXISTS (SELECT 1 FROM forge_audit.ddl_audit_ack AS a WHERE a.ddl_audit_id = d.id)
    FROM forge_audit.ddl_audit AS d
    CROSS JOIN LATERAL (
        SELECT CASE WHEN d.object_type IN ('trigger', 'rule', 'policy', 'table constraint')
                    THEN pg_catalog.split_part(d.object_identity, ' on ', 2)
                    ELSE d.parent_identity END AS parent,
               d.object_type IN ('trigger', 'rule', 'policy', 'table constraint') AS child
    ) AS c
    CROSS JOIN LATERAL (
        SELECT EXISTS (
            SELECT 1 FROM forge_audit.guarded_objects AS go
            WHERE go.object_identity IN (d.object_identity, c.parent)
        ) AS guarded
    ) AS g
    CROSS JOIN LATERAL (
        SELECT pg_catalog.array_remove(ARRAY[
            d.object_identity,
            pg_catalog.regexp_replace(d.object_identity, '\(.*\)$', ''),
            CASE WHEN c.child
                 THEN c.parent || '.' || pg_catalog.split_part(d.object_identity, ' on ', 1)
                 ELSE c.parent END
        ], NULL) AS names
    ) AS n
    CROSS JOIN LATERAL (
        SELECT pg_catalog.count(al.txid) > 0 AS present,
               coalesce(pg_catalog.jsonb_agg(e.value) FILTER (WHERE e.value IS NOT NULL),
                        '[]'::jsonb) AS changes
        FROM forge.audit_log AS al
        LEFT JOIN LATERAL pg_catalog.jsonb_array_elements(
            coalesce(al.details -> 'guarded_changes', '[]'::jsonb)) AS e ON true
        WHERE al.txid = d.txid AND al.action = 'db.migrate'
    ) AS m;
END
$$;
ALTER FUNCTION forge_audit.ddl_audit_status() OWNER TO forge_audit;
REVOKE ALL ON FUNCTION forge_audit.ddl_audit_status() FROM PUBLIC, forge_owner, forge_migrator,
    forge_seq, forge_purge, forge_app, forge_ro, forge_maint;
GRANT EXECUTE ON FUNCTION forge_audit.ddl_audit_status() TO forge_app, forge_ro, forge_maint;
-- CREATE OR REPLACE keeps an existing owner: set it again.
ALTER FUNCTION forge_audit.cluster_identity() OWNER TO CURRENT_USER;

-- Schema forge_audit holds only what this file creates.
DO $$
DECLARE
    x record;
BEGIN
    FOR x IN
        SELECT e.extname FROM pg_catalog.pg_extension e
        JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace
        WHERE n.nspname = 'forge_audit'
    LOOP
        RAISE WARNING 'forge bootstrap: dropping unexpected extension % in forge_audit', x.extname;
        EXECUTE format('DROP EXTENSION %I', x.extname);
    END LOOP;
    FOR x IN
        SELECT p.oid::pg_catalog.regprocedure AS sig FROM pg_catalog.pg_proc p
        JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
        WHERE n.nspname = 'forge_audit'
          AND p.oid NOT IN ('forge_audit.reject_log_mutation()'::pg_catalog.regprocedure,
                            'forge_audit.ddl_audit_record()'::pg_catalog.regprocedure,
                            'forge_audit.cluster_identity()'::pg_catalog.regprocedure,
                            'forge_audit.ddl_audit_status()'::pg_catalog.regprocedure)
    LOOP
        RAISE WARNING 'forge bootstrap: dropping unexpected function % in forge_audit', x.sig;
        EXECUTE format('DROP FUNCTION %s', x.sig);
    END LOOP;
END
$$;

-- ── Guarded objects created here ───────────────────────────────────────
INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('table', 'forge_audit.bootstrap_log'),
    ('table', 'forge_audit.ddl_audit'),
    ('table', 'forge_audit.ddl_audit_ack'),
    ('table', 'forge_audit.guarded_objects'),
    ('table', 'forge._sqlx_migrations'),
    ('function', 'forge_audit.reject_log_mutation()'),
    ('function', 'forge_audit.ddl_audit_record()'),
    ('function', 'forge_audit.cluster_identity()'),
    ('function', 'forge_audit.ddl_audit_status()'),
    ('event trigger', 'forge_ddl_audit_end'),
    ('event trigger', 'forge_ddl_audit_drop')
ON CONFLICT DO NOTHING;

-- Same transaction as the DDL above: ddl_audit rows with this txid are the
-- bootstrap's own (§3). Bump the version when this file changes; doctor
-- (db.bootstrap) requires the version its binary was built with.
INSERT INTO forge_audit.bootstrap_log (bootstrap_version) VALUES (5);

COMMIT;
