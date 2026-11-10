-- M4 C7 (plan 14 §4.1, §5 C7; plan 04 M-1 / M-2; ADR-0024 Amendment
-- 2026-11-11): the merge results of merge requests.
--
-- forge.merge_result_refs (derived): one row per (merge request, head,
-- target tip) the mergeability job found clean — the commit forge wrote with
-- `git commit-tree` (parents: the target tip, then the head; the tree of
-- `merge-tree --write-tree`) and pointed `refs/merge-requests/<iid>/merge`
-- at. The row is written once per pair (merge_result_refs_once): a second
-- check of the same pair reuses result_sha and writes no commit; the first
-- insert of a pair also enqueues the merge request pipeline on result_sha
-- (ci.pipeline_create, source merge_request) in the same transaction.
-- Earlier rows of a head stay: a required check reported on an earlier
-- result_sha of the head is STALE (FG-CI-0013 RequiredCheckStale), never
-- "missing" and never counted. A conflict writes no row (and the ref is
-- deleted). Recomputable from git and the merge request, so no audit row.
--   head_sha / target_sha  the pair merged (target_sha = the target tip)
--   result_sha             the merge commit
--
-- forge_app writes it (the mergeability job) and deletes it in the project
-- purge (projects/purge.rs); the read pool reads every column (the merge
-- status).

CREATE TABLE forge.merge_result_refs (
    id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    merge_request_id  bigint NOT NULL REFERENCES forge.merge_requests (id),
    project_id        bigint NOT NULL REFERENCES forge.projects (id),
    head_sha          text NOT NULL CONSTRAINT merge_result_refs_head
        CHECK (head_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    target_sha        text NOT NULL CONSTRAINT merge_result_refs_target
        CHECK (target_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    result_sha        text NOT NULL CONSTRAINT merge_result_refs_result
        CHECK (result_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    created_at        timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT merge_result_refs_once UNIQUE (merge_request_id, head_sha, target_sha)
);
-- The project purge deletes a project's rows.
CREATE INDEX merge_result_refs_project ON forge.merge_result_refs (project_id);

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.merge_result_refs TO forge_app;
GRANT SELECT ON forge.merge_result_refs TO forge_ro;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261207100000, "guarded_changes": []}');
