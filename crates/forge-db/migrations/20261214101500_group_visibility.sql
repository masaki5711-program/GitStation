-- Plan 15 MR 2: groups.visibility — who may read a group besides the
-- people holding a role on its chain ('private' = members only,
-- 'internal' = every signed-in user), the words projects.visibility
-- already uses (ADR-0010). The effective visibility of a group or project
-- is the narrowest value on its chain up to the top level; the role
-- loaders compute it (forge_db::roles::load_*_access, one statement with
-- the roles). Nothing decides with it yet: authorize() starts to in plan
-- 15 MR 3, so this migration changes no answer.
--
-- Existing groups: DEFAULT 'internal' keeps today's behaviour (every
-- signed-in user sees every group). ADD COLUMN with a constant default is
-- a catalog change (no table rewrite, no UPDATE, so no audit row is needed
-- and no row trigger fires); this file's own `db.migrate` row is its trace.
--
-- Audited like every other groups column: the audit trigger is re-created
-- with visibility in UPDATE OF, so a later change needs a `group` audit
-- row in its transaction (tables.toml [tables.groups]).
--
-- Lock impact: ACCESS EXCLUSIVE on forge.groups for the ALTER, the CHECK
-- validation (one scan of a small table) and the trigger swap, until
-- COMMIT. Accepted: groups is small and this runs once at deploy.

ALTER TABLE forge.groups ADD COLUMN visibility text NOT NULL DEFAULT 'internal'
    CONSTRAINT groups_visibility CHECK (visibility IN ('private', 'internal'));

DROP TRIGGER audited_change_present ON forge.groups;
CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, display_name, description, created_at, visibility
    ON forge.groups
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('group', 'id', 'updated_at', '');
ALTER TABLE forge.groups ENABLE ALWAYS TRIGGER audited_change_present;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261214101500, "guarded_changes": ["forge.groups.audited_change_present"]}');
