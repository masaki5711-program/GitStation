-- ADR-0021 amendment 2026-11-24 (管理者を Keycloak のロールで付け外し):
-- users.admin_source — who manages the account's administrator flag.
--
--   'cli'   the default: the flag is set and cleared by forge's own paths
--           (`forge admin bootstrap-admin`, `forge admin user admin-source`,
--           the restore's regrant). The IdP's admin role never clears it,
--           so a break-glass administrator survives a sign-in without the
--           role. For a user who is not an administrator 'cli' simply means
--           "not granted by the IdP".
--   'oidc'  the flag was granted by `auth.oidc.admin_role` at a sign-in or
--           a refresh check, and the next sign-in or refresh check without
--           the role clears it (back to 'cli'). Only while is_admin.
--
-- Existing rows: DEFAULT 'cli' keeps today's behaviour (every existing
-- administrator was made by the CLI). ADD COLUMN with a constant default
-- is a catalog change (no rewrite, no UPDATE: no audit row is needed and
-- no row trigger fires); this file's own `db.migrate` row is its trace.
--
-- Audited like is_admin: the audit trigger is re-created with
-- admin_source in UPDATE OF, so a change needs a `user` audit row in its
-- transaction (tables.toml [tables.users]).
--
-- Lock impact: ACCESS EXCLUSIVE on forge.users for the ALTERs, the CHECK
-- validation (one scan of a small table) and the trigger swap, until
-- COMMIT; bounded by the migrator's lock_timeout (database.lock_timeout).

ALTER TABLE forge.users ADD COLUMN admin_source text NOT NULL DEFAULT 'cli'
    CONSTRAINT users_admin_source CHECK (admin_source IN ('cli', 'oidc'));
ALTER TABLE forge.users ADD CONSTRAINT users_admin_source_only_admin CHECK (
    admin_source = 'cli' OR is_admin);

DROP TRIGGER audited_change_present ON forge.users;
CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, kind, username, display_name, email, employee_number,
        external_id, auth_source, state, is_admin, created_at, admin_source ON forge.users
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('user', 'id', 'updated_at', '');
ALTER TABLE forge.users ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT (admin_source) ON forge.users TO forge_ro;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20270109100000, "guarded_changes": ["forge.users.audited_change_present"]}');
