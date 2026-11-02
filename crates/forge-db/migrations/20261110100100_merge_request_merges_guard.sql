-- M3 R4 review round 1 (ADR-0024 §4.2; "実装で決めたこと（R4）"): the merge
-- attempts' guard and the reconciliation's order. Appended instead of editing
-- 20261110100000 (append-only, ADR-0014).
--
-- 1. merge_request_merges_guard now also guards INSERT: an attempt starts
--    `queued`, with no `expected_old` / `new_sha` / `executing_at` / reason /
--    `source_outcome` / `finished_at` (the state machine has one entry), and
--    an `applied` attempt is frozen except `source_outcome` (set once, the
--    source branch deletion after the merge) and `updated_at`. The trigger
--    merge_request_merges_forward fires BEFORE INSERT OR UPDATE (recreated).
--
-- 2. reconcile_seen_at: when the reconciliation last looked at a live attempt
--    without settling it (its repository's merge lock was busy, or an error
--    was logged). The scan takes the attempts never looked at first, then the
--    longest unseen, so a few attempts it cannot settle never starve the rest
--    (head-of-line blocking of a fixed `ORDER BY id`). Operational only.

ALTER TABLE forge.merge_request_merges ADD COLUMN reconcile_seen_at timestamptz;

CREATE OR REPLACE FUNCTION forge.merge_request_merges_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.state IS DISTINCT FROM 'queued'
           OR NEW.expected_old IS NOT NULL OR NEW.new_sha IS NOT NULL
           OR NEW.executing_at IS NOT NULL OR NEW.reason IS NOT NULL
           OR NEW.error_code IS NOT NULL OR NEW.source_outcome IS NOT NULL
           OR NEW.finished_at IS NOT NULL THEN
            RAISE EXCEPTION USING ERRCODE = 'XF001',
                MESSAGE = 'a merge attempt on forge.merge_request_merges starts queued';
        END IF;
        RETURN NEW;
    END IF;
    IF NEW.merge_request_id IS DISTINCT FROM OLD.merge_request_id
       OR NEW.project_id IS DISTINCT FROM OLD.project_id
       OR NEW.method IS DISTINCT FROM OLD.method
       OR NEW.requested_by IS DISTINCT FROM OLD.requested_by
       OR NEW.credential_kind IS DISTINCT FROM OLD.credential_kind
       OR NEW.credential_id IS DISTINCT FROM OLD.credential_id
       OR NEW.request_id IS DISTINCT FROM OLD.request_id
       OR NEW.head_sha IS DISTINCT FROM OLD.head_sha
       OR NEW.target_branch IS DISTINCT FROM OLD.target_branch
       OR NEW.message IS DISTINCT FROM OLD.message
       OR NEW.delete_source_branch IS DISTINCT FROM OLD.delete_source_branch
       OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the identity of a merge attempt on forge.merge_request_merges never changes';
    END IF;
    IF (OLD.expected_old IS NOT NULL AND NEW.expected_old IS DISTINCT FROM OLD.expected_old)
       OR (OLD.new_sha IS NOT NULL AND NEW.new_sha IS DISTINCT FROM OLD.new_sha)
       OR (OLD.executing_at IS NOT NULL AND NEW.executing_at IS DISTINCT FROM OLD.executing_at)
       OR (OLD.source_outcome IS NOT NULL
           AND NEW.source_outcome IS DISTINCT FROM OLD.source_outcome) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'expected_old, new_sha, executing_at and source_outcome of a merge attempt on forge.merge_request_merges are set once';
    END IF;
    IF NEW.state IS DISTINCT FROM OLD.state AND NOT (
           (OLD.state = 'queued' AND NEW.state IN ('prepared', 'superseded', 'failed'))
           OR (OLD.state = 'prepared' AND NEW.state IN ('executing', 'superseded', 'failed'))
           OR (OLD.state = 'executing' AND NEW.state IN ('applied', 'superseded', 'failed'))) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a merge attempt on forge.merge_request_merges only moves forward, and a final state never changes';
    END IF;
    IF OLD.state IN ('superseded', 'failed') AND NEW IS DISTINCT FROM OLD THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a merge attempt on forge.merge_request_merges never changes once superseded or failed';
    END IF;
    IF OLD.state = 'applied' AND (
           NEW.state IS DISTINCT FROM OLD.state
           OR NEW.expected_old IS DISTINCT FROM OLD.expected_old
           OR NEW.new_sha IS DISTINCT FROM OLD.new_sha
           OR NEW.executing_at IS DISTINCT FROM OLD.executing_at
           OR NEW.reason IS DISTINCT FROM OLD.reason
           OR NEW.error_code IS DISTINCT FROM OLD.error_code
           OR NEW.finished_at IS DISTINCT FROM OLD.finished_at
           OR NEW.reconcile_seen_at IS DISTINCT FROM OLD.reconcile_seen_at) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'an applied merge attempt on forge.merge_request_merges changes only its source_outcome';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.merge_request_merges_guard() FROM PUBLIC;

DROP TRIGGER merge_request_merges_forward ON forge.merge_request_merges;
CREATE TRIGGER merge_request_merges_forward BEFORE INSERT OR UPDATE ON forge.merge_request_merges
    FOR EACH ROW EXECUTE FUNCTION forge.merge_request_merges_guard();
ALTER TABLE forge.merge_request_merges ENABLE ALWAYS TRIGGER merge_request_merges_forward;

-- The reconciliation's scan of live attempts, never looked at first.
CREATE INDEX merge_request_merges_reconcile
    ON forge.merge_request_merges (reconcile_seen_at NULLS FIRST, id)
    WHERE state IN ('queued', 'prepared', 'executing');

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261110100100, "guarded_changes": ["forge.merge_request_merges_guard", "forge.merge_request_merges.merge_request_merges_forward"]}');
