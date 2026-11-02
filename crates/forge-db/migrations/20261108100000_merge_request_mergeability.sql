-- M3 R2 (ADR-0024 §3; "実装で決めたこと（R2）"): whether a merge request
-- can be merged, as the mergeability job last computed it.
--
-- merge_request_mergeability: one row per merge request that ever had a
-- check requested. DERIVED state (tables.toml kind = "derived"): it is
-- recomputed from git at any time and is not a decision of a person, so a
-- change needs no audit row — which is why it is not a column of
-- merge_requests (the revision guard 20261106100100 makes every change of
-- that row bump `revision` and its audited trigger demands an audit row;
-- the job and the subscriber write this status many times per merge
-- request). The events that matter (merge_request.conflict_detected /
-- .conflict_resolved, written only on a can ⇄ cannot flip) carry their own
-- audit row in the same transaction.
--
--   status            unchecked | checking | can_be_merged | cannot_be_merged
--                     | check_failed (only can / cannot are decisions;
--                     everything else blocks a merge as mergeability_unknown)
--   reason            why: `conflict`, `target_missing`, `unrelated_histories`,
--                     `no_version`, `git_timeout`, `git_busy`, `git_failed`
--   check_seq         bumped by every request; a job writes only while its own
--                     seq is the latest (jobs are deduplicated per seq, so a
--                     target going A → B → A is checked again: the jobs dedup
--                     index keeps succeeded jobs forever, 20261027120000)
--   checked_head_sha / checked_target_sha / checked_at
--                     the pair the last finished check looked at
--   conflicts         the conflicted paths (capped by the job), with
--                     conflicts_truncated when more exist
--   last_decided      the last decision (can / cannot): a flip is measured
--                     against it, never against an undecided status
--
-- forge_app writes it (the subscriber, the commands, the job) and deletes it
-- in the project purge (projects/purge.rs).

CREATE TABLE forge.merge_request_mergeability (
    merge_request_id     bigint PRIMARY KEY REFERENCES forge.merge_requests (id),
    project_id           bigint NOT NULL REFERENCES forge.projects (id),
    status               text NOT NULL DEFAULT 'unchecked'
        CONSTRAINT merge_request_mergeability_status CHECK (status IN
            ('unchecked', 'checking', 'can_be_merged', 'cannot_be_merged', 'check_failed')),
    reason               text CONSTRAINT merge_request_mergeability_reason
        CHECK (reason ~ '^[a-z][a-z0-9_]{0,63}$'),
    check_seq            bigint NOT NULL DEFAULT 1
        CONSTRAINT merge_request_mergeability_check_seq CHECK (check_seq > 0),
    checked_head_sha     text CONSTRAINT merge_request_mergeability_checked_head
        CHECK (checked_head_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    checked_target_sha   text CONSTRAINT merge_request_mergeability_checked_target
        CHECK (checked_target_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    checked_at           timestamptz,
    conflicts            text[] NOT NULL DEFAULT '{}'
        CONSTRAINT merge_request_mergeability_conflicts CHECK (
            cardinality(conflicts) <= 1000
            AND octet_length(array_to_string(conflicts, '/')) <= 65536),
    conflicts_truncated  boolean NOT NULL DEFAULT false,
    last_decided         text CONSTRAINT merge_request_mergeability_last_decided
        CHECK (last_decided IN ('can_be_merged', 'cannot_be_merged')),
    updated_at           timestamptz NOT NULL DEFAULT now()
);
-- The project purge deletes a project's rows.
CREATE INDEX merge_request_mergeability_project
    ON forge.merge_request_mergeability (project_id);

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.merge_request_mergeability TO forge_app;
GRANT SELECT ON forge.merge_request_mergeability TO forge_ro;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261108100000, "guarded_changes": []}');
