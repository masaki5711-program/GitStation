-- Step D5 MR3 review: a repository enters `purging` only when its project
-- is pending deletion and the grace period is over (purge_after <= now).
-- Restore (D5 MR3) locks the repository row and reads its state; the purge
-- job (MR4) writes `purging`. With this check the database, not the two
-- callers' good manners, makes "exactly one wins": a project that was
-- restored (or never deleted) can never have its repository moved to
-- `purging`, and a purge start that waits on a restore's repository-row
-- lock re-evaluates against the committed restore and is refused (XF001,
-- FG-DB-0009 reason=rejected).
--
-- The check reads the project row without locking it, so it takes no lock
-- in the opposite order of restore (project, then repository). The MR4
-- purge job should still lock the project row first and re-check
-- deleted_at / purge_after itself.

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
    IF NEW.state = 'purging' AND OLD.state IS DISTINCT FROM 'purging'
       AND NOT EXISTS (
            SELECT 1 FROM forge.projects AS p
            WHERE p.id = NEW.project_id
              AND p.deleted_at IS NOT NULL
              AND p.purge_after <= clock_timestamp()) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a repository on forge.repositories enters purging only when its project is pending deletion and its grace period is over';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.repositories_state_guard() FROM PUBLIC;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261102100200, "guarded_changes": ["forge.repositories_state_guard"]}');
