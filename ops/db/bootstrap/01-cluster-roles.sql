-- forge bootstrap step 1 (ADR-0018 §1): cluster roles and memberships.
-- Run as a superuser, connected to the maintenance database:
--   psql -d postgres -f 01-cluster-roles.sql
-- Idempotent: re-running enforces the attributes and memberships again.
-- A role with one of these names that this file did not create (no
-- 'forge bootstrap role' comment) stops the run: roles are cluster-wide
-- and may belong to another database (ops/runbooks/db-bootstrap.md).
-- Passwords are not set here; set them per the runbook.
-- Concurrent runs connected to the same database serialize on the advisory
-- lock (advisory locks are per database; a run from another database
-- fails on the duplicate role and can be retried).
\set ON_ERROR_STOP on

BEGIN;

-- Resolve names in pg_catalog only (no overload planted in public runs here).
SET LOCAL search_path = pg_catalog, pg_temp;

SELECT pg_catalog.pg_advisory_xact_lock(pg_catalog.hashtext('forge.bootstrap.cluster_roles'));

DO $$
DECLARE
    r record;
    marker constant text := 'forge bootstrap role';
BEGIN
    FOR r IN
        SELECT * FROM (VALUES
            ('forge_owner', false),
            ('forge_migrator', true),
            ('forge_seq', false),
            ('forge_purge', false),
            ('forge_app', true),
            ('forge_ro', false),
            ('forge_maint', true),
            ('forge_audit', false)
        ) AS t(name, login)
    LOOP
        IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = r.name) THEN
            EXECUTE format('CREATE ROLE %I', r.name);
            EXECUTE format('COMMENT ON ROLE %I IS %L', r.name, marker);
        ELSIF pg_catalog.shobj_description(
                  (SELECT oid FROM pg_catalog.pg_roles WHERE rolname = r.name),
                  'pg_authid') IS DISTINCT FROM marker THEN
            RAISE EXCEPTION USING
                MESSAGE = format('role %s exists but was not created by the forge bootstrap', r.name),
                HINT = 'review the role; to adopt it run COMMENT ON ROLE ' || r.name
                    || ' IS ''forge bootstrap role'' and re-run (ops/runbooks/db-bootstrap.md)';
        END IF;
        EXECUTE format(
            'ALTER ROLE %I %s NOSUPERUSER NOCREATEROLE NOCREATEDB NOBYPASSRLS NOREPLICATION',
            r.name,
            CASE WHEN r.login THEN 'LOGIN' ELSE 'NOLOGIN' END
        );
    END LOOP;
END
$$;

-- Memberships carry SET only: a login has no privileges of the granted role
-- until it switches to it (SET ROLE), so the effective role is explicit.
-- forge_audit has no members at all: it owns the DDL log and the guard
-- functions that a superuser-owned event trigger calls, so nothing below
-- the superuser can attach code to them.
GRANT forge_owner TO forge_migrator WITH INHERIT FALSE, SET TRUE;
-- The owner hands function ownership to forge_seq / forge_purge
-- (ALTER FUNCTION ... OWNER TO needs SET on the new owner).
GRANT forge_seq TO forge_owner WITH INHERIT FALSE, SET TRUE;
GRANT forge_purge TO forge_owner WITH INHERIT FALSE, SET TRUE;
-- The read pool: forge_app logs in and runs as forge_ro.
GRANT forge_ro TO forge_app WITH INHERIT FALSE, SET TRUE;

COMMIT;
