-- Step D5 MR2 (ADR-0015 Amendment D5, ADR-0016 Amendment D5): what moves,
-- deletion and purge need from the schema.
--
-- projects.deleted_by: who deleted a project pending deletion (D5 MR3
-- writes it with deleted_at / purge_after; it becomes the owner_at_write of
-- the deletion tombstones). NULL exactly when deleted_at is NULL is not
-- enforced: the operator CLI deletes as `system`, with no user. Audited like
-- every other projects column (the audit trigger is re-created with it).
--
-- repositories.state gains `purging` (D5 MR4: the purge job removes the
-- bare directory, then the rows). The guard lets the state move
-- provisioning -> ready, and provisioning | ready -> purging; nothing comes
-- back from purging, and ready_at, once set, never changes. ready_at is set
-- exactly when `ready`, never when `provisioning`; a purging repository
-- keeps what it had.
--
-- DELETE for forge_app on namespaces, projects and repositories (D5 MR4
-- purge deletes the rows, step D5 plan answer "purge deletes the rows");
-- project_members and protected_refs have it since D4. TRUNCATE for no one.
--
-- namespace_tombstones (cause_type, cause_id, created_at): the move rate
-- limit (10 moves per namespace per 24 h, counted from the `moved`
-- tombstones a move writes for its cause) reads it.

ALTER TABLE forge.projects ADD COLUMN deleted_by bigint REFERENCES forge.users (id);

DROP TRIGGER audited_change_present ON forge.projects;
CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, display_name, description, visibility, created_at,
        deleted_at, purge_after, deleted_by ON forge.projects
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'id', 'updated_at', '');
ALTER TABLE forge.projects ENABLE ALWAYS TRIGGER audited_change_present;

ALTER TABLE forge.repositories DROP CONSTRAINT repositories_state;
ALTER TABLE forge.repositories ADD CONSTRAINT repositories_state
    CHECK (state IN ('provisioning', 'ready', 'purging'));
ALTER TABLE forge.repositories DROP CONSTRAINT repositories_ready_at;
ALTER TABLE forge.repositories ADD CONSTRAINT repositories_ready_at CHECK (
    (state <> 'ready' OR ready_at IS NOT NULL)
    AND (state <> 'provisioning' OR ready_at IS NULL));

CREATE OR REPLACE FUNCTION forge.repositories_state_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF (NEW.state IS DISTINCT FROM OLD.state
            AND NOT ((OLD.state = 'provisioning' AND NEW.state = 'ready')
                OR (OLD.state IN ('provisioning', 'ready') AND NEW.state = 'purging')))
       OR (OLD.ready_at IS NOT NULL AND NEW.ready_at IS DISTINCT FROM OLD.ready_at) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the state of a repository on forge.repositories moves only from provisioning to ready, and from provisioning or ready to purging';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.repositories_state_guard() FROM PUBLIC;

CREATE INDEX namespace_tombstones_cause
    ON forge.namespace_tombstones (cause_type, cause_id, created_at);

GRANT DELETE ON forge.namespaces TO forge_app;
GRANT DELETE ON forge.projects TO forge_app;
GRANT DELETE ON forge.repositories TO forge_app;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261102100000, "guarded_changes": ["forge.projects.audited_change_present", "forge.repositories_state_guard"]}');
