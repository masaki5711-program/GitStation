-- M3 R1a review (database-reviewer): the events of a merge request are
-- deduplicated by <prefix>:<merge_request_id>:<revision>, so every change of
-- a row must add exactly one to its revision — a change that kept it (or
-- skipped one) could collide with, or be mistaken for, another change.
-- merge_requests_state_guard (20261106100000) gains that rule; the rest of
-- the guard is unchanged (merged is final, closed is reopened before it is
-- merged, the project / iid / author / source branch never change).
-- Appended instead of editing 20261106100000 (append-only, ADR-0014);
-- doctor compares the function with this, its last CREATE OR REPLACE.

CREATE OR REPLACE FUNCTION forge.merge_requests_state_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NEW.state IS DISTINCT FROM OLD.state
       AND (OLD.state = 'merged' OR (OLD.state = 'closed' AND NEW.state = 'merged')) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a merge request on forge.merge_requests never leaves merged, and is reopened before it is merged';
    END IF;
    IF NEW.project_id IS DISTINCT FROM OLD.project_id OR NEW.iid IS DISTINCT FROM OLD.iid
       OR NEW.author_id IS DISTINCT FROM OLD.author_id
       OR NEW.source_branch IS DISTINCT FROM OLD.source_branch THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the project, number, author and source branch of a merge request on forge.merge_requests never change';
    END IF;
    IF NEW IS DISTINCT FROM OLD AND NEW.revision IS DISTINCT FROM OLD.revision + 1 THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'every change of a merge request on forge.merge_requests adds one to its revision';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.merge_requests_state_guard() FROM PUBLIC;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261106100100, "guarded_changes": ["forge.merge_requests_state_guard"]}');
