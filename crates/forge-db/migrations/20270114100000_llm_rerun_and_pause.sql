-- Plan 19 §8.2 (ADR-0039 Amendment `feat/core-llm-settings-and-rerun`):
-- the routes of `AiReviewRerun`, `LlmPause` and `LlmResume`.
--
-- forge.llm_review_reruns — one row per accepted rerun of a merge
--   request's AI first pass (`POST …/ai-review/rerun`): the count record
--   (at most 3 per merge request in a rolling hour, counted under the
--   merge request's row lock) and the worker's signal. A head whose
--   `forge/ai-review` run is completed is asked again only while a rerun
--   of that head is newer than the run (the worker's queued write makes
--   the run newer, so one rerun asks once). Each row is also one
--   `llm.review_rerun_requested` audit row. Purged with the project.
--     head_sha      the head the person saw (the merge request's head then)
--     requested_by  the person
--     request_id    the request that asked
--
-- forge.llm_gateway — the instance's pause of the LLM gateway (one row,
--   id = 1): set by an administrator (`POST /api/v1/admin/llm/pause` /
--   `…/resume`, audited `llm.paused` / `llm.resumed`); the worker reads it
--   every 10 s and is paused while it or `llm.paused` says so.

CREATE TABLE forge.llm_review_reruns (
    id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    merge_request_id bigint NOT NULL REFERENCES forge.merge_requests (id),
    project_id       bigint NOT NULL REFERENCES forge.projects (id),
    head_sha         text NOT NULL CONSTRAINT llm_review_reruns_head_sha
        CHECK (head_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    requested_by     bigint NOT NULL REFERENCES forge.users (id),
    request_id       text NOT NULL CONSTRAINT llm_review_reruns_request_id
        CHECK (char_length(request_id) BETWEEN 1 AND 200),
    requested_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX llm_review_reruns_mr_time
    ON forge.llm_review_reruns (merge_request_id, requested_at);
CREATE INDEX llm_review_reruns_project ON forge.llm_review_reruns (project_id);
CREATE INDEX llm_review_reruns_requested_by ON forge.llm_review_reruns (requested_by);

CREATE TABLE forge.llm_gateway (
    id         smallint PRIMARY KEY CONSTRAINT llm_gateway_one_row CHECK (id = 1),
    paused     boolean NOT NULL DEFAULT false,
    changed_by bigint REFERENCES forge.users (id),
    changed_at timestamptz
);
CREATE INDEX llm_gateway_changed_by ON forge.llm_gateway (changed_by);
INSERT INTO forge.llm_gateway (id) VALUES (1);

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.llm_review_reruns TO forge_app;
GRANT SELECT ON forge.llm_review_reruns TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.llm_gateway TO forge_app;
GRANT SELECT ON forge.llm_gateway TO forge_ro;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20270114100000, "guarded_changes": []}');
