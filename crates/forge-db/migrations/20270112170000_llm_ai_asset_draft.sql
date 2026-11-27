-- Plan 17 P17-10 (ADR-0039 Amendment P17-10): the local LLM's AI asset
-- draft is a new purpose of the gateway's records. The worker's
-- `asset_draft` task asks for a fix of a skill or a playbook from a draft
-- improvement merge request's case and proposes it on `ai-draft/<id>`;
-- its requests, advice and feedback rows carry purpose `ai_asset_draft`.
--
-- The three purpose constraints gain it (replaced at once: the tables are
-- derived records, small, and nothing else changes). No new table.
ALTER TABLE forge.llm_requests DROP CONSTRAINT llm_requests_purpose;
ALTER TABLE forge.llm_requests ADD CONSTRAINT llm_requests_purpose
    CHECK (purpose IN ('mr_review', 'incident_diagnosis', 'approval_summary', 'ai_asset_draft'));
ALTER TABLE forge.llm_advice DROP CONSTRAINT llm_advice_purpose;
ALTER TABLE forge.llm_advice ADD CONSTRAINT llm_advice_purpose
    CHECK (purpose IN ('mr_review', 'incident_diagnosis', 'approval_summary', 'ai_asset_draft'));
ALTER TABLE forge.llm_advice_feedback DROP CONSTRAINT llm_advice_feedback_purpose;
ALTER TABLE forge.llm_advice_feedback ADD CONSTRAINT llm_advice_feedback_purpose
    CHECK (purpose IN ('mr_review', 'incident_diagnosis', 'approval_summary', 'ai_asset_draft'));

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20270112170000, "guarded_changes": []}');
