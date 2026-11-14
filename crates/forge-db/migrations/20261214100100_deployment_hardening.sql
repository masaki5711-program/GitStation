-- M4 C11 review (deploy-safety-reviewer, security-reviewer; ADR-0031
-- amendment 2026-11-13): what an approver sees and who may not approve.
--
-- forge.deployments gains
--   commit_subject / commit_author_name / commit_author_email /
--   committer_email   the commit as read from git when it was requested
--                     (NULL for an automatic request): shown to the
--                     approver; the author and committer, mapped to
--                     accounts by email, may not approve it.
--   heartbeat_at      written by the worker while it runs the deployment;
--                     a `running` deployment whose heartbeat stopped is
--                     ended `failed` by the sweep (FG-DEPLOY-0012).
-- The guard keeps the commit fields with the identity: set at the insert,
-- never changed.

ALTER TABLE forge.deployments
    ADD COLUMN commit_subject text CONSTRAINT deployments_commit_subject
        CHECK (octet_length(commit_subject) <= 1024),
    ADD COLUMN commit_author_name text CONSTRAINT deployments_commit_author_name
        CHECK (octet_length(commit_author_name) <= 256),
    ADD COLUMN commit_author_email text CONSTRAINT deployments_commit_author_email
        CHECK (octet_length(commit_author_email) <= 320),
    ADD COLUMN committer_email text CONSTRAINT deployments_committer_email
        CHECK (octet_length(committer_email) <= 320),
    ADD COLUMN heartbeat_at timestamptz;

CREATE INDEX deployments_running_heartbeat ON forge.deployments (heartbeat_at)
    WHERE status = 'running';

CREATE OR REPLACE FUNCTION forge.deployments_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NEW.project_id IS DISTINCT FROM OLD.project_id
       OR NEW.environment_id IS DISTINCT FROM OLD.environment_id
       OR NEW.tier IS DISTINCT FROM OLD.tier
       OR NEW.sha IS DISTINCT FROM OLD.sha
       OR NEW.requested_by IS DISTINCT FROM OLD.requested_by
       OR NEW.request_id IS DISTINCT FROM OLD.request_id
       OR NEW.merge_request_author IS DISTINCT FROM OLD.merge_request_author
       OR NEW.commit_subject IS DISTINCT FROM OLD.commit_subject
       OR NEW.commit_author_name IS DISTINCT FROM OLD.commit_author_name
       OR NEW.commit_author_email IS DISTINCT FROM OLD.commit_author_email
       OR NEW.committer_email IS DISTINCT FROM OLD.committer_email
       OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the identity of a deployment on forge.deployments never changes';
    END IF;
    IF (OLD.approved_by IS NOT NULL AND (NEW.approved_by IS DISTINCT FROM OLD.approved_by
            OR NEW.approved_at IS DISTINCT FROM OLD.approved_at
            OR NEW.approval_expires_at IS DISTINCT FROM OLD.approval_expires_at))
       OR (OLD.previous_sha IS NOT NULL AND NEW.previous_sha IS DISTINCT FROM OLD.previous_sha)
       OR (OLD.started_at IS NOT NULL AND NEW.started_at IS DISTINCT FROM OLD.started_at) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the approval, previous_sha and started_at of a deployment on forge.deployments are set once';
    END IF;
    IF NEW.status IS DISTINCT FROM OLD.status AND NOT (
           (OLD.status = 'pending_approval' AND NEW.status IN ('queued', 'blocked'))
           OR (OLD.status = 'queued' AND NEW.status IN ('running', 'blocked'))
           OR (OLD.status = 'running'
               AND NEW.status IN ('succeeded', 'failed', 'rolled_back', 'rollback_failed'))) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a deployment on forge.deployments only moves forward, and a final state never changes';
    END IF;
    IF OLD.status IN ('succeeded', 'failed', 'rolled_back', 'rollback_failed', 'blocked')
       AND NEW IS DISTINCT FROM OLD THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a finished deployment on forge.deployments never changes';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.deployments_guard() FROM PUBLIC;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261214100100, "guarded_changes": ["forge.deployments_guard"]}');
