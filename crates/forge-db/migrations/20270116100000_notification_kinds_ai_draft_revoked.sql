-- Plan 17 P17-10 (ADR-0039 Amendment P17-10b) and plan 16 A9's 残すこと
-- (ADR-0032 Amendment A13): two notification kinds.
--
--   mr_ai_draft       merge_request.ai_draft_proposed — the author of a
--                     draft improvement merge request is told that the
--                     local LLM proposed a fix of an AI asset on its own
--                     branch ai-draft/<id> (a proposal; nothing applied).
--   mr_agent_revoked  merge_request.agent_update_opened with origin
--                     revocation — every Maintainer and Owner of the
--                     project besides the author is told that the update
--                     merge request for a revoked part version waits for a
--                     Maintainer's approval.
--
-- The mail and the in-app CHECKs are replaced in this file's one
-- transaction (the tables are small: one row per person, one per
-- delivery), the same form as 20270113100000.

ALTER TABLE forge.notification_preferences
    DROP CONSTRAINT notification_preferences_muted_kinds;
ALTER TABLE forge.notification_preferences
    ADD CONSTRAINT notification_preferences_muted_kinds CHECK (
        muted_kinds <@ ARRAY['mr_approved', 'mr_merged', 'mr_commented',
            'mr_conflict_predicted', 'mr_revert_drafted', 'mr_agent_update',
            'mr_agent_revoked', 'mr_ai_draft']::text[]
        AND cardinality(muted_kinds) <= 8);

ALTER TABLE forge.notification_preferences
    DROP CONSTRAINT notification_preferences_in_app_muted_kinds;
ALTER TABLE forge.notification_preferences
    ADD CONSTRAINT notification_preferences_in_app_muted_kinds CHECK (
        in_app_muted_kinds <@ ARRAY['mr_approved', 'mr_merged', 'mr_commented',
            'mr_conflict_predicted', 'mr_revert_drafted', 'mr_agent_update',
            'mr_agent_revoked', 'mr_ai_draft']::text[]
        AND cardinality(in_app_muted_kinds) <= 8);

ALTER TABLE forge.notifications DROP CONSTRAINT notifications_kind;
ALTER TABLE forge.notifications
    ADD CONSTRAINT notifications_kind CHECK (kind IN
        ('mr_approved', 'mr_merged', 'mr_commented', 'mr_conflict_predicted',
         'mr_revert_drafted', 'mr_agent_update', 'mr_agent_revoked', 'mr_ai_draft'));

ALTER TABLE forge.in_app_notifications DROP CONSTRAINT in_app_notifications_kind;
ALTER TABLE forge.in_app_notifications
    ADD CONSTRAINT in_app_notifications_kind CHECK (kind IN
        ('mr_approved', 'mr_merged', 'mr_commented', 'mr_conflict_predicted',
         'mr_revert_drafted', 'mr_agent_update', 'mr_agent_revoked', 'mr_ai_draft'));

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20270116100000, "guarded_changes": []}');
