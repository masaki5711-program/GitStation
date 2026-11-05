-- M3 N1 (plan 11 §5 N1, ADR-0016 Amendment N1): e-mail notifications.
--
-- 1. forge.notification_preferences (a person's own settings): e-mail on or
--    off, the language of the mail (NULL: the instance default), and the
--    kinds muted. One row per person, written only by that person's own
--    route; no row means the defaults (on, instance language, nothing
--    muted). Not audited: it changes no one's access and no one else's
--    data, and the route is session / token bound to its owner.
-- 2. forge.notifications (derived): one row per (source event, recipient),
--    written by the `notify` subscriber in the same transaction as its
--    progress, so a retried step can never add a second row
--    (notifications_one_per_event). The row is the delivery record: its
--    state says whether the mail was sent, is waiting, or why none went
--    out. The mail body is built at send time from live data and never
--    stored. No foreign key to forge.projects or forge.events: the project
--    purge deletes projects (the row then reads as "gone" and is
--    suppressed) and events are pruned after their retention period.
--    Grants: forge_app reads, inserts and updates (never deletes);
--    forge_ro reads.
-- 3. The delivery itself runs as the job kind `notification.send`
--    (forge.jobs: lease, backoff, dead-letter, doctor job health); the job
--    carries only the notification's id.

CREATE TABLE forge.notification_preferences (
    user_id       bigint PRIMARY KEY REFERENCES forge.users (id),
    email_enabled boolean NOT NULL DEFAULT true,
    language      text
        CONSTRAINT notification_preferences_language CHECK (language IN ('ja', 'en', 'id')),
    muted_kinds   text[] NOT NULL DEFAULT '{}'
        CONSTRAINT notification_preferences_muted_kinds CHECK (
            muted_kinds <@ ARRAY['mr_approved', 'mr_merged', 'mr_commented']::text[]
            AND cardinality(muted_kinds) <= 3),
    updated_at    timestamptz NOT NULL DEFAULT now()
);
GRANT SELECT, INSERT, UPDATE ON forge.notification_preferences TO forge_app;
GRANT SELECT ON forge.notification_preferences TO forge_ro;

CREATE TABLE forge.notifications (
    id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id           bigint NOT NULL REFERENCES forge.users (id),
    kind              text NOT NULL
        CONSTRAINT notifications_kind CHECK (kind IN ('mr_approved', 'mr_merged', 'mr_commented')),
    -- The source event (no foreign key: events are pruned).
    event_id          uuid NOT NULL,
    event_seq         bigint NOT NULL CONSTRAINT notifications_event_seq CHECK (event_seq > 0),
    project_id        bigint NOT NULL CONSTRAINT notifications_project CHECK (project_id > 0),
    merge_request_iid bigint NOT NULL CONSTRAINT notifications_iid CHECK (merge_request_iid > 0),
    actor_user_id     bigint REFERENCES forge.users (id),
    state             text NOT NULL DEFAULT 'pending'
        CONSTRAINT notifications_state CHECK (state IN
            ('pending', 'sending', 'retrying', 'sent', 'failed', 'unknown', 'skipped', 'suppressed')),
    -- Why the state is what it is (a short code, never free text).
    reason            text
        CONSTRAINT notifications_reason_shape CHECK (reason ~ '^[a-z][a-z0-9_]{0,39}$'),
    attempts          integer NOT NULL DEFAULT 0
        CONSTRAINT notifications_attempts CHECK (attempts BETWEEN 0 AND 25),
    request_id        text NOT NULL
        CONSTRAINT notifications_request_id CHECK (length(request_id) BETWEEN 1 AND 128),
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    sent_at           timestamptz,
    CONSTRAINT notifications_one_per_event UNIQUE (event_id, user_id),
    CONSTRAINT notifications_sent_at CHECK ((state = 'sent') = (sent_at IS NOT NULL)),
    -- A notification that went no further says why.
    CONSTRAINT notifications_terminal_reason CHECK (
        state NOT IN ('failed', 'unknown', 'skipped', 'suppressed') OR reason IS NOT NULL)
);
-- The doctor's backlog and the oldest waiting one.
CREATE INDEX notifications_open ON forge.notifications (created_at)
    WHERE state IN ('pending', 'sending', 'retrying');
-- The doctor's recent failures.
CREATE INDEX notifications_problems ON forge.notifications (updated_at)
    WHERE state IN ('failed', 'unknown');
GRANT SELECT, INSERT, UPDATE ON forge.notifications TO forge_app;
GRANT SELECT ON forge.notifications TO forge_ro;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261120100000, "guarded_changes": []}');
