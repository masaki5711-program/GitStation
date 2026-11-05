-- M3 X6 (plan 04 C-2, ADR-0024 Amendment X6): the notification kind
-- `mr_conflict_predicted` — the author of each merge request of a pair the
-- conflict scan predicts to conflict is told (merge_request.conflict_predicted).
-- Both tables are small (one row per person, one per delivery); the CHECKs
-- are replaced in this file's one transaction.

ALTER TABLE forge.notification_preferences
    DROP CONSTRAINT notification_preferences_muted_kinds;
ALTER TABLE forge.notification_preferences
    ADD CONSTRAINT notification_preferences_muted_kinds CHECK (
        muted_kinds <@ ARRAY['mr_approved', 'mr_merged', 'mr_commented',
            'mr_conflict_predicted']::text[]
        AND cardinality(muted_kinds) <= 4);

ALTER TABLE forge.notifications DROP CONSTRAINT notifications_kind;
ALTER TABLE forge.notifications
    ADD CONSTRAINT notifications_kind CHECK (kind IN
        ('mr_approved', 'mr_merged', 'mr_commented', 'mr_conflict_predicted'));

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261123100100, "guarded_changes": []}');
