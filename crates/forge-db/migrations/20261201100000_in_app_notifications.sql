-- M3 U4 (plan 11 §5.7 U4, ADR-0016 Amendment U4): in-app notifications.
--
-- 1. forge.notification_preferences gains the in-app switches beside the
--    mail ones: `in_app_enabled` (all in-app items on or off) and
--    `in_app_muted_kinds` (the kinds not shown in the app). The defaults
--    (on, nothing muted) are what a person without a row already gets.
-- 2. forge.in_app_notifications (derived): one row per (source event,
--    recipient), written by the same `notify` subscriber in the same step
--    as the mail record and for the same recipient (the merge request's
--    author), so a retried step can never add a second row
--    (in_app_notifications_one_per_event). `read_at` is the only column a
--    person changes (their own, through the route; not audited: it changes
--    no one's access and no one else's data). Who may see the item is
--    decided again on every read (forge_auth::prove on MergeRequestRead),
--    never stored. No foreign key to forge.projects or forge.events (the
--    project purge and the event retention delete those; the item then
--    reads as not visible and is not listed).
--    Retention (the worker's `in_app_notification_retention` task): read
--    items 30 days after they were read, every item 180 days after it was
--    created. Grants: forge_app reads, inserts, updates and deletes;
--    forge_ro reads.

ALTER TABLE forge.notification_preferences
    ADD COLUMN in_app_enabled boolean NOT NULL DEFAULT true,
    ADD COLUMN in_app_muted_kinds text[] NOT NULL DEFAULT '{}'
        CONSTRAINT notification_preferences_in_app_muted_kinds CHECK (
            in_app_muted_kinds <@ ARRAY['mr_approved', 'mr_merged', 'mr_commented',
                'mr_conflict_predicted']::text[]
            AND cardinality(in_app_muted_kinds) <= 4);

CREATE TABLE forge.in_app_notifications (
    id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id           bigint NOT NULL REFERENCES forge.users (id),
    kind              text NOT NULL
        CONSTRAINT in_app_notifications_kind CHECK (kind IN
            ('mr_approved', 'mr_merged', 'mr_commented', 'mr_conflict_predicted')),
    -- The source event (no foreign key: events are pruned).
    event_id          uuid NOT NULL,
    event_seq         bigint NOT NULL CONSTRAINT in_app_notifications_event_seq CHECK (event_seq > 0),
    project_id        bigint NOT NULL CONSTRAINT in_app_notifications_project CHECK (project_id > 0),
    merge_request_iid bigint NOT NULL CONSTRAINT in_app_notifications_iid CHECK (merge_request_iid > 0),
    actor_user_id     bigint REFERENCES forge.users (id),
    created_at        timestamptz NOT NULL DEFAULT now(),
    read_at           timestamptz,
    CONSTRAINT in_app_notifications_one_per_event UNIQUE (event_id, user_id),
    CONSTRAINT in_app_notifications_read_after_created CHECK (read_at IS NULL OR read_at >= created_at)
);
-- A person's list, newest first (the keyset is the id).
CREATE INDEX in_app_notifications_by_user ON forge.in_app_notifications (user_id, id DESC);
-- The unread count and the unread filter.
CREATE INDEX in_app_notifications_unread ON forge.in_app_notifications (user_id, id DESC)
    WHERE read_at IS NULL;
-- The retention.
CREATE INDEX in_app_notifications_read_at ON forge.in_app_notifications (read_at)
    WHERE read_at IS NOT NULL;
CREATE INDEX in_app_notifications_created_at ON forge.in_app_notifications (created_at);
-- The foreign key to the actor (a user row is never deleted today; the index
-- keeps the key's check cheap if one ever is, as for the recipient's).
CREATE INDEX in_app_notifications_actor ON forge.in_app_notifications (actor_user_id)
    WHERE actor_user_id IS NOT NULL;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.in_app_notifications TO forge_app;
GRANT SELECT ON forge.in_app_notifications TO forge_ro;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261201100000, "guarded_changes": []}');
