-- Plan 16 A9 (ADR-0032 Amendment A9): the agent builder's update merge
-- requests — asked by a person (`POST …/agent-builder/update`) or opened
-- for every project whose lock uses a part version that was revoked.
--
-- forge.agent_builds (derived) gains `origin`: `build` (the A7 builder,
-- every row before this file), `update` (a person's update) or
-- `revocation` (the job opened after a revocation). Its partial GIN index
-- finds the projects whose finished builds selected a part version
-- (`selection @> '[{"slug": …, "version": …}]'`, the revocation's
-- fan-out, inside the revocation's transaction). The table is small and
-- new (A7); the index is built in this file's transaction.
--
-- The notification kind `mr_agent_update` (merge_request.agent_update_opened:
-- the person the update was opened for is told). The mail and the in-app
-- CHECKs are replaced in this file's one transaction (small tables: one
-- row per person, one per delivery).

ALTER TABLE forge.agent_builds
    ADD COLUMN origin text NOT NULL DEFAULT 'build'
        CONSTRAINT agent_builds_origin CHECK (origin IN ('build', 'update', 'revocation'));

CREATE INDEX agent_builds_selection ON forge.agent_builds
    USING gin (selection jsonb_path_ops)
    WHERE state = 'succeeded' AND NOT dry_run;

ALTER TABLE forge.notification_preferences
    DROP CONSTRAINT notification_preferences_muted_kinds;
ALTER TABLE forge.notification_preferences
    ADD CONSTRAINT notification_preferences_muted_kinds CHECK (
        muted_kinds <@ ARRAY['mr_approved', 'mr_merged', 'mr_commented',
            'mr_conflict_predicted', 'mr_revert_drafted', 'mr_agent_update']::text[]
        AND cardinality(muted_kinds) <= 6);

ALTER TABLE forge.notification_preferences
    DROP CONSTRAINT notification_preferences_in_app_muted_kinds;
ALTER TABLE forge.notification_preferences
    ADD CONSTRAINT notification_preferences_in_app_muted_kinds CHECK (
        in_app_muted_kinds <@ ARRAY['mr_approved', 'mr_merged', 'mr_commented',
            'mr_conflict_predicted', 'mr_revert_drafted', 'mr_agent_update']::text[]
        AND cardinality(in_app_muted_kinds) <= 6);

ALTER TABLE forge.notifications DROP CONSTRAINT notifications_kind;
ALTER TABLE forge.notifications
    ADD CONSTRAINT notifications_kind CHECK (kind IN
        ('mr_approved', 'mr_merged', 'mr_commented', 'mr_conflict_predicted',
         'mr_revert_drafted', 'mr_agent_update'));

ALTER TABLE forge.in_app_notifications DROP CONSTRAINT in_app_notifications_kind;
ALTER TABLE forge.in_app_notifications
    ADD CONSTRAINT in_app_notifications_kind CHECK (kind IN
        ('mr_approved', 'mr_merged', 'mr_commented', 'mr_conflict_predicted',
         'mr_revert_drafted', 'mr_agent_update'));

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20270113100000, "guarded_changes": []}');
