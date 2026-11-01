-- Step D5 MR2 review: the DELETE grants of 20261102100000 are for the
-- purge only, so a guard narrows them (for every role, ENABLE ALWAYS;
-- XF001, FG-DB-0009 reason=rejected; doctor compares the triggers and the
-- function body, bootstrap/schema.rs):
--   repositories: only a repository in state `purging`;
--   projects: only a project pending deletion (deleted_at set);
--   namespaces: only a project namespace whose project is pending deletion
--     or already gone (user and group namespaces are never deleted in M1).
--
-- audit_log_moves: the move rate limit (10 per namespace per 24 h) counts
-- the resource's own move audit rows (append-only, so releasing a
-- tombstone cannot lower the count). Partial: move rows only.

CREATE FUNCTION forge.purge_delete_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_TABLE_NAME = 'repositories' THEN
        IF OLD.state <> 'purging' THEN
            RAISE EXCEPTION USING ERRCODE = 'XF001',
                MESSAGE = 'a repository on forge.repositories is deleted only in state purging';
        END IF;
    ELSIF TG_TABLE_NAME = 'projects' THEN
        IF OLD.deleted_at IS NULL THEN
            RAISE EXCEPTION USING ERRCODE = 'XF001',
                MESSAGE = 'a project on forge.projects is deleted only when pending deletion';
        END IF;
    ELSIF TG_TABLE_NAME = 'namespaces' THEN
        IF OLD.kind <> 'project' OR EXISTS (
            SELECT 1 FROM forge.projects AS p
            WHERE p.id = OLD.owner_id AND p.deleted_at IS NULL
        ) THEN
            RAISE EXCEPTION USING ERRCODE = 'XF001',
                MESSAGE = 'a namespace on forge.namespaces is deleted only for a purged project';
        END IF;
    ELSE
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'forge.purge_delete_guard is attached to an unknown table';
    END IF;
    RETURN OLD;
END
$$;
REVOKE ALL ON FUNCTION forge.purge_delete_guard() FROM PUBLIC;

CREATE TRIGGER repositories_purge_only BEFORE DELETE ON forge.repositories
    FOR EACH ROW EXECUTE FUNCTION forge.purge_delete_guard();
ALTER TABLE forge.repositories ENABLE ALWAYS TRIGGER repositories_purge_only;
CREATE TRIGGER projects_purge_only BEFORE DELETE ON forge.projects
    FOR EACH ROW EXECUTE FUNCTION forge.purge_delete_guard();
ALTER TABLE forge.projects ENABLE ALWAYS TRIGGER projects_purge_only;
CREATE TRIGGER namespaces_purge_only BEFORE DELETE ON forge.namespaces
    FOR EACH ROW EXECUTE FUNCTION forge.purge_delete_guard();
ALTER TABLE forge.namespaces ENABLE ALWAYS TRIGGER namespaces_purge_only;

CREATE INDEX audit_log_moves ON forge.audit_log (resource_type, resource_id, occurred_at)
    WHERE action IN ('project.moved', 'group.moved', 'user.renamed');

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('function', 'forge.purge_delete_guard()'),
    ('trigger', 'repositories_purge_only on forge.repositories'),
    ('trigger', 'projects_purge_only on forge.projects'),
    ('trigger', 'namespaces_purge_only on forge.namespaces')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261102100100, "guarded_changes": ["forge.purge_delete_guard", "forge.repositories.repositories_purge_only", "forge.projects.projects_purge_only", "forge.namespaces.namespaces_purge_only", "forge.audit_log"]}');
