-- M3 R4 (ADR-0024 §4, §8.5; plan 11 §4.1 R4, §11 decisions 9 and 10):
-- merging a merge request.
--
-- 1. The token scope `merge_execute` (ADR-0024 §8.5, ADR-0017 §4.0): a merge
--    needs it and `api` does not satisfy it. tokens_scopes_known
--    (20261030100000) accepted five values; it is replaced by the same CHECK
--    with the sixth (the variant and its CHECK land in the same MR, ADR-0024
--    §8.5; `merge_approve` joins with R5). Appended instead of editing the
--    tokens migration (append-only, ADR-0014).
--
-- 2. merge_request_merges: one row per merge attempt (`merge_id` = id). The
--    states (ADR-0024 §4.2):
--      queued     the request was accepted and the job queued (nothing built)
--      prepared   the objects exist; expected_old (the target tip the trial
--                 merge used) and new_sha (the commit the target moves to)
--                 are fixed; the ref has not moved
--      executing  the attempt won the fence (prepared -> executing,
--                 executing_at): it may have moved the ref — only git can
--                 tell (the reconciliation asks it)
--      applied    final: the ref moved and the merge is recorded
--      superseded final: the ref did not move (refused, conflict, lost
--                 compare-and-swap, stale)
--      failed     final: a person decides (runbook merge-execution)
--    At most one live (queued / prepared / executing) attempt per merge
--    request: merge_request_merges_one_live backs FG-REVIEW-0011
--    MergeInProgress (checked first under the merge request's row lock).
--    The requester is stored as the user and the credential's kind and id
--    only (ADR-0024 §4.2 step 1, §5): scopes, the agent flag, the account
--    kind and roles are read again from the database by the job, never
--    copied. `message` is the requester's commit message (NULL: built from
--    the merge request's title and description). `source_outcome` records
--    what happened to the source branch when its deletion was asked
--    (set once, after `applied`).
--
--    DERIVED state (tables.toml kind = "derived"): the operational record of
--    a job, like merge_request_mergeability. What a person decided — the
--    merge itself — is audited where it lands: `merge_request.merged` on the
--    project (with `repository.pushed` for the target) in the transaction
--    that sets `applied` and the merge request `merged`; the source branch
--    deletion likewise. The guard merge_request_merges_guard holds the state
--    machine (forge_core::MergeState::may_become): final states never
--    change, live states only move forward, the identity of an attempt never
--    changes, and expected_old / new_sha / executing_at are set once.
--
-- forge_app writes it (the request route, the merge job, the reconciliation)
-- and deletes it in the project purge (projects/purge.rs), before the merge
-- requests.

ALTER TABLE forge.tokens DROP CONSTRAINT tokens_scopes_known;
ALTER TABLE forge.tokens ADD CONSTRAINT tokens_scopes_known CHECK (
    cardinality(scopes) >= 1
    AND scopes <@ ARRAY['read_repository', 'write_repository', 'read_api', 'api', 'admin',
        'merge_execute']::text[]);

CREATE TABLE forge.merge_request_merges (
    id                    bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    merge_request_id      bigint NOT NULL REFERENCES forge.merge_requests (id),
    project_id            bigint NOT NULL REFERENCES forge.projects (id),
    method                text NOT NULL CONSTRAINT merge_request_merges_method
        CHECK (method IN ('merge_commit', 'squash', 'fast_forward')),
    state                 text NOT NULL DEFAULT 'queued' CONSTRAINT merge_request_merges_state
        CHECK (state IN ('queued', 'prepared', 'executing', 'applied', 'superseded', 'failed')),
    requested_by          bigint NOT NULL REFERENCES forge.users (id),
    credential_kind       text NOT NULL CONSTRAINT merge_request_merges_credential_kind
        CHECK (credential_kind IN ('session', 'token')),
    credential_id         bigint NOT NULL,
    request_id            text NOT NULL CONSTRAINT merge_request_merges_request_id
        CHECK (char_length(request_id) BETWEEN 1 AND 128),
    head_sha              text NOT NULL CONSTRAINT merge_request_merges_head_sha
        CHECK (head_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    target_branch         text COLLATE "C" NOT NULL CONSTRAINT merge_request_merges_target_branch
        CHECK (target_branch ~ '^refs/heads/.' AND octet_length(target_branch) <= 1024),
    message               text CONSTRAINT merge_request_merges_message
        CHECK (char_length(message) >= 1 AND octet_length(message) <= 65536),
    delete_source_branch  boolean NOT NULL,
    expected_old          text CONSTRAINT merge_request_merges_expected_old
        CHECK (expected_old ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    new_sha               text CONSTRAINT merge_request_merges_new_sha
        CHECK (new_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    executing_at          timestamptz,
    reason                text CONSTRAINT merge_request_merges_reason
        CHECK (reason ~ '^[a-z][a-z0-9_]{0,63}$'),
    error_code            text CONSTRAINT merge_request_merges_error_code
        CHECK (error_code ~ '^FG-[A-Z]+-[0-9]{4}$'),
    source_outcome        text CONSTRAINT merge_request_merges_source_outcome
        CHECK (source_outcome IN ('deleted', 'kept_refused', 'kept_moved', 'kept_failed')),
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now(),
    finished_at           timestamptz,
    CONSTRAINT merge_request_merges_prepared_fields CHECK (
        (state IN ('queued', 'superseded', 'failed'))
        OR (expected_old IS NOT NULL AND new_sha IS NOT NULL)),
    CONSTRAINT merge_request_merges_executing_fields CHECK (
        state NOT IN ('executing', 'applied') OR executing_at IS NOT NULL),
    CONSTRAINT merge_request_merges_finished CHECK (
        (state IN ('applied', 'superseded', 'failed')) = (finished_at IS NOT NULL)),
    CONSTRAINT merge_request_merges_source_outcome_applied CHECK (
        source_outcome IS NULL OR (state = 'applied' AND delete_source_branch))
);
CREATE UNIQUE INDEX merge_request_merges_one_live
    ON forge.merge_request_merges (merge_request_id)
    WHERE state IN ('queued', 'prepared', 'executing');
-- The latest attempt of a merge request (the status route), the project purge
-- and the reconciliation's scan of live attempts.
CREATE INDEX merge_request_merges_merge_request
    ON forge.merge_request_merges (merge_request_id, id);
CREATE INDEX merge_request_merges_project ON forge.merge_request_merges (project_id);
CREATE INDEX merge_request_merges_live
    ON forge.merge_request_merges (updated_at)
    WHERE state IN ('queued', 'prepared', 'executing');

CREATE FUNCTION forge.merge_request_merges_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
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
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.merge_request_merges_guard() FROM PUBLIC;
CREATE TRIGGER merge_request_merges_forward BEFORE UPDATE ON forge.merge_request_merges
    FOR EACH ROW EXECUTE FUNCTION forge.merge_request_merges_guard();
ALTER TABLE forge.merge_request_merges ENABLE ALWAYS TRIGGER merge_request_merges_forward;

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.merge_request_merges TO forge_app;
GRANT SELECT ON forge.merge_request_merges TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('function', 'forge.merge_request_merges_guard()'),
    ('trigger', 'merge_request_merges_forward on forge.merge_request_merges')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261110100000, "guarded_changes": ["forge.merge_request_merges_guard", "forge.merge_request_merges.merge_request_merges_forward"]}');
