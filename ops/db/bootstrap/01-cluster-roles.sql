-- forge bootstrap step 1 (ADR-0018 §1): cluster roles and memberships.
-- Run as a superuser, connected to the maintenance database:
--   psql -v ON_ERROR_STOP=1 -d postgres -f 01-cluster-roles.sql
-- Idempotent: re-running enforces the attributes and memberships again.
-- Passwords are not set here; set them per ops/runbooks/db-bootstrap.md.
-- Concurrent runs connected to the same database serialize on the advisory
-- lock (advisory locks are per database).

BEGIN;

SELECT pg_advisory_xact_lock(hashtext('forge.bootstrap.cluster_roles'));

DO $$
DECLARE
    r record;
BEGIN
    FOR r IN
        SELECT * FROM (VALUES
            ('forge_owner', false),
            ('forge_migrator', true),
            ('forge_seq', false),
            ('forge_purge', false),
            ('forge_app', true),
            ('forge_ro', false),
            ('forge_maint', true)
        ) AS t(name, login)
    LOOP
        IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = r.name) THEN
            EXECUTE format('CREATE ROLE %I', r.name);
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
GRANT forge_owner TO forge_migrator WITH INHERIT FALSE, SET TRUE;
-- The owner hands function ownership to forge_seq / forge_purge
-- (ALTER FUNCTION ... OWNER TO needs SET on the new owner).
GRANT forge_seq TO forge_owner WITH INHERIT FALSE, SET TRUE;
GRANT forge_purge TO forge_owner WITH INHERIT FALSE, SET TRUE;
-- The read pool: forge_app logs in and runs as forge_ro.
GRANT forge_ro TO forge_app WITH INHERIT FALSE, SET TRUE;

COMMIT;
