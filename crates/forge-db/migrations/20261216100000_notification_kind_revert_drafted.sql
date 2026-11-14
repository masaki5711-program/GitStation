-- M4 C10 (plan 14 C10, plan 04 M-4, ADR-0024 Amendment C10): the
-- notification kind `mr_revert_drafted` — the author of a merge request is
-- told when the target branch's push pipeline failed on the commit its
-- merge produced and the system drafted a revert (or could not: the revert
-- conflicts) — merge_request.revert_drafted. The mail and the in-app
-- CHECKs are replaced in this file's one transaction (the tables are small:
-- one row per person, one per delivery).

ALTER TABLE forge.notification_preferences
    DROP CONSTRAINT notification_preferences_muted_kinds;
ALTER TABLE forge.notification_preferences
    ADD CONSTRAINT notification_preferences_muted_kinds CHECK (
        muted_kinds <@ ARRAY['mr_approved', 'mr_merged', 'mr_commented',
            'mr_conflict_predicted', 'mr_revert_drafted']::text[]
        AND cardinality(muted_kinds) <= 5);

ALTER TABLE forge.notification_preferences
    DROP CONSTRAINT notification_preferences_in_app_muted_kinds;
ALTER TABLE forge.notification_preferences
    ADD CONSTRAINT notification_preferences_in_app_muted_kinds CHECK (
        in_app_muted_kinds <@ ARRAY['mr_approved', 'mr_merged', 'mr_commented',
            'mr_conflict_predicted', 'mr_revert_drafted']::text[]
        AND cardinality(in_app_muted_kinds) <= 5);

ALTER TABLE forge.notifications DROP CONSTRAINT notifications_kind;
ALTER TABLE forge.notifications
    ADD CONSTRAINT notifications_kind CHECK (kind IN
        ('mr_approved', 'mr_merged', 'mr_commented', 'mr_conflict_predicted',
         'mr_revert_drafted'));

ALTER TABLE forge.in_app_notifications DROP CONSTRAINT in_app_notifications_kind;
ALTER TABLE forge.in_app_notifications
    ADD CONSTRAINT in_app_notifications_kind CHECK (kind IN
        ('mr_approved', 'mr_merged', 'mr_commented', 'mr_conflict_predicted',
         'mr_revert_drafted'));

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261216100000, "guarded_changes": []}');
