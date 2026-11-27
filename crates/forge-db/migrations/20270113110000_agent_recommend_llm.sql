-- Plan 16 A11 (ADR-0032 Amendment A11, ADR-0039 Amendment A11): the local
-- LLM gives reasons for the agent builder's rules-only recommendation from
-- the repository's specification documents.
--
-- 1. The gateway's records gain the purpose `agent_recommend` and the
--    subject kind `project` (the advice is about a project's default
--    branch tip: head_sha). The constraints are replaced at once (the
--    tables are derived records, small; nothing else changes).
-- 2. forge.agent_recommend_asks (derived) — one row per project whose
--    recommendation was read: the route (`POST …/agent-builder/recommend`,
--    a separate process from the worker's gateway) upserts it, the worker's
--    `agent_recommend` task answers it.
--      asked_at       the newest read (the worker looks at recent asks only)
--      pending_since  when the row last became `pending` (a reader calls an
--                     ask that no worker answers `unavailable` after a grace)
--      state          pending | ready | unavailable | paused | refused
--      head_sha       the default branch tip the worker answered for
--      error_code     the FG code of `unavailable` / `refused` (when one)
--      advice_id      the advice of `ready` (the advice's own row decides
--                     who reads its words)
--      request_id     the reader's request that asked last
--    Deleted with its project by the purge. No input is stored (plan 19 §5).
ALTER TABLE forge.llm_requests DROP CONSTRAINT llm_requests_purpose;
ALTER TABLE forge.llm_requests ADD CONSTRAINT llm_requests_purpose
    CHECK (purpose IN ('mr_review', 'incident_diagnosis', 'approval_summary', 'ai_asset_draft',
                       'agent_recommend'));
ALTER TABLE forge.llm_advice DROP CONSTRAINT llm_advice_purpose;
ALTER TABLE forge.llm_advice ADD CONSTRAINT llm_advice_purpose
    CHECK (purpose IN ('mr_review', 'incident_diagnosis', 'approval_summary', 'ai_asset_draft',
                       'agent_recommend'));
ALTER TABLE forge.llm_advice_feedback DROP CONSTRAINT llm_advice_feedback_purpose;
ALTER TABLE forge.llm_advice_feedback ADD CONSTRAINT llm_advice_feedback_purpose
    CHECK (purpose IN ('mr_review', 'incident_diagnosis', 'approval_summary', 'ai_asset_draft',
                       'agent_recommend'));
ALTER TABLE forge.llm_requests DROP CONSTRAINT llm_requests_subject_kind;
ALTER TABLE forge.llm_requests ADD CONSTRAINT llm_requests_subject_kind
    CHECK (subject_kind IN ('merge_request', 'deployment', 'incident', 'project'));
ALTER TABLE forge.llm_advice DROP CONSTRAINT llm_advice_subject_kind;
ALTER TABLE forge.llm_advice ADD CONSTRAINT llm_advice_subject_kind
    CHECK (subject_kind IN ('merge_request', 'deployment', 'incident', 'project'));

CREATE TABLE forge.agent_recommend_asks (
    project_id     bigint PRIMARY KEY REFERENCES forge.projects (id),
    asked_at       timestamptz NOT NULL DEFAULT now(),
    pending_since  timestamptz NOT NULL DEFAULT now(),
    state          text NOT NULL DEFAULT 'pending' CONSTRAINT agent_recommend_asks_state
        CHECK (state IN ('pending', 'ready', 'unavailable', 'paused', 'refused')),
    head_sha       text CONSTRAINT agent_recommend_asks_head_sha
        CHECK (head_sha ~ '^[0-9a-f]{40}([0-9a-f]{24})?$'),
    error_code     text CONSTRAINT agent_recommend_asks_error_code
        CHECK (error_code ~ '^FG-[A-Z]+-[0-9]{4}$'),
    advice_id      bigint REFERENCES forge.llm_advice (id) ON DELETE SET NULL,
    request_id     text NOT NULL CONSTRAINT agent_recommend_asks_request_id
        CHECK (char_length(request_id) BETWEEN 1 AND 128),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT agent_recommend_asks_ready_has_head
        CHECK (state <> 'ready' OR head_sha IS NOT NULL)
);
CREATE INDEX agent_recommend_asks_asked_at ON forge.agent_recommend_asks (asked_at);
CREATE INDEX agent_recommend_asks_advice ON forge.agent_recommend_asks (advice_id);
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.agent_recommend_asks TO forge_app;
GRANT SELECT ON forge.agent_recommend_asks TO forge_ro;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20270113110000, "guarded_changes": []}');
