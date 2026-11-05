-- M3 I2 (plan 11 §5 I2, ADR-0016 §8 and its Amendment I2, plan 02 §5.5 /
-- §11.1): project webhooks and their delivery history.
--
-- 1. forge.webhooks (audited on ('project', project_id)): one push receiver
--    of a project, at most 20 per project (the command counts under a
--    per-project lock, FG-INT-0012). `owner_user_id` is the person who
--    created it: every delivery is decided again on the owner's standing
--    (Maintainer of the project, the event's class), so a webhook stops
--    when its owner loses the project (FG-INT-0010 reason=owner_lost).
--    The URL passed forge-core's target policy when stored; the delivery
--    checks it again against the policy of the moment and the addresses
--    the name resolves to then. The secret is sealed with AES-256-GCM under
--    a key of webhook.secret_keys_file (outside the database, ADR-0013),
--    the associated data binding it to `uid`; during the 24 hours after a
--    rotation the previous secret signs too (`secret_prev_*`). The read
--    pool never sees a ciphertext, nor the full `url` (its `origin` only).
--    Changing the URL or event types, or rotating the secret, makes the
--    caller the owner (audited). The delivery counters
--    (`consecutive_failures`, `failing_since`, `last_delivery_at`,
--    `updated_at`) are unaudited; disabling (by a person, or by the worker
--    after 50 failed attempts in a row or 24 hours of failures,
--    `disabled_reason`) is audited (`webhook.disabled` by the system also
--    publishes the event). Deleted by a person (webhook.deleted) or by the
--    project purge.
--
-- 2. forge.webhook_deliveries (derived): one row per (webhook, event)
--    (webhook_deliveries_once: the `webhook-dispatch` subscriber writes it
--    in the same transaction as its progress, so a retried step never adds
--    a second one). `delivery_uid` is the Forge-Delivery value. The row
--    carries the state and the last attempt's status code, reason and FG
--    code only — never a response body or the URL. Retention (the worker,
--    webhook.delivery_retention_days, default 30) deletes finished rows;
--    deleting a webhook deletes its rows. No foreign key to forge.events
--    (events are pruned by their own retention).

CREATE TABLE forge.webhooks (
    id                     bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    uid                    uuid NOT NULL CONSTRAINT webhooks_uid_unique UNIQUE,
    project_id             bigint NOT NULL REFERENCES forge.projects (id),
    owner_user_id          bigint NOT NULL REFERENCES forge.users (id),
    url                    text NOT NULL CONSTRAINT webhooks_url_shape
        CHECK (length(url) BETWEEN 10 AND 2048
            AND url ~ '^https?://[^/?#@[:space:]]+([/?][^#[:space:]]*)?$'),
    -- scheme://host:port of `url`: what reads, logs and audit rows show (the
    -- path or query of `url` may hold a receiver's token: write-only).
    origin                 text NOT NULL CONSTRAINT webhooks_origin_shape
        CHECK (origin ~ '^https?://[^/?#@[:space:]]+:[0-9]{1,5}$'),
    event_types            text[] NOT NULL CONSTRAINT webhooks_event_types_entries
        CHECK (cardinality(event_types) BETWEEN 1 AND 64
            AND array_ndims(event_types) = 1
            AND array_position(event_types, NULL) IS NULL
            AND cardinality(string_to_array(array_to_string(event_types, ' '), ' '))
                = cardinality(event_types)
            AND (' ' || array_to_string(event_types, ' '))
                ~ '^( (\*|[a-z_]+(\.[a-z_]+)*(\.\*)?))+$'),
    enabled                boolean NOT NULL DEFAULT true,
    disabled_reason        text CONSTRAINT webhooks_disabled_reason_known
        CHECK (disabled_reason IN ('manual', 'failures', 'failing_24h')),
    secret_key_id          smallint NOT NULL CONSTRAINT webhooks_secret_key_id
        CHECK (secret_key_id > 0),
    secret_ciphertext      bytea NOT NULL CONSTRAINT webhooks_secret_ciphertext
        CHECK (octet_length(secret_ciphertext) BETWEEN 28 AND 256),
    secret_version         integer NOT NULL DEFAULT 1 CONSTRAINT webhooks_secret_version
        CHECK (secret_version >= 1),
    secret_prev_key_id     smallint,
    secret_prev_ciphertext bytea,
    secret_prev_until      timestamptz,
    consecutive_failures   integer NOT NULL DEFAULT 0 CONSTRAINT webhooks_failures
        CHECK (consecutive_failures >= 0),
    failing_since          timestamptz,
    last_delivery_at       timestamptz,
    created_at             timestamptz NOT NULL DEFAULT now(),
    updated_at             timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT webhooks_disabled_consistent CHECK (enabled = (disabled_reason IS NULL)),
    CONSTRAINT webhooks_previous_secret_complete CHECK (
        (secret_prev_key_id IS NULL) = (secret_prev_ciphertext IS NULL)
        AND (secret_prev_key_id IS NULL) = (secret_prev_until IS NULL)),
    CONSTRAINT webhooks_failing_since CHECK ((consecutive_failures = 0) = (failing_since IS NULL))
);
CREATE INDEX webhooks_project ON forge.webhooks (project_id, id);

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, uid, project_id, owner_user_id, url, origin, event_types,
        enabled, disabled_reason, secret_key_id, secret_ciphertext, secret_version,
        secret_prev_key_id, secret_prev_ciphertext, secret_prev_until, created_at
    ON forge.webhooks
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id',
        'consecutive_failures,failing_since,last_delivery_at,updated_at', '');
ALTER TABLE forge.webhooks ENABLE ALWAYS TRIGGER audited_change_present;

CREATE TABLE forge.webhook_deliveries (
    id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    delivery_uid     uuid NOT NULL CONSTRAINT webhook_deliveries_uid_unique UNIQUE,
    webhook_id       bigint NOT NULL REFERENCES forge.webhooks (id) ON DELETE CASCADE,
    project_id       bigint NOT NULL CONSTRAINT webhook_deliveries_project CHECK (project_id > 0),
    -- The source event (no foreign key: events are pruned).
    event_seq        bigint NOT NULL CONSTRAINT webhook_deliveries_event_seq CHECK (event_seq > 0),
    event_id         uuid NOT NULL,
    event_type       text NOT NULL CONSTRAINT webhook_deliveries_event_type
        CHECK (event_type ~ '^[a-z][a-z_]*(\.[a-z][a-z_]*){1,3}$'),
    state            text NOT NULL DEFAULT 'pending' CONSTRAINT webhook_deliveries_state
        CHECK (state IN ('pending', 'succeeded', 'failed', 'skipped')),
    attempts         integer NOT NULL DEFAULT 0 CONSTRAINT webhook_deliveries_attempts
        CHECK (attempts BETWEEN 0 AND 1000),
    last_status      integer CONSTRAINT webhook_deliveries_last_status
        CHECK (last_status BETWEEN 100 AND 599),
    -- A short code, never free text (forge-integ's reasons).
    last_reason      text CONSTRAINT webhook_deliveries_last_reason
        CHECK (last_reason ~ '^[a-z][a-z0-9_]{0,39}$'),
    last_error_code  text CONSTRAINT webhook_deliveries_last_error_code
        CHECK (last_error_code ~ '^FG-[A-Z]+-[0-9]{4}$'),
    last_duration_ms integer CONSTRAINT webhook_deliveries_duration
        CHECK (last_duration_ms >= 0),
    redeliveries     integer NOT NULL DEFAULT 0 CONSTRAINT webhook_deliveries_redeliveries
        CHECK (redeliveries >= 0),
    request_id       text NOT NULL CONSTRAINT webhook_deliveries_request_id
        CHECK (length(request_id) BETWEEN 1 AND 128),
    created_at       timestamptz NOT NULL DEFAULT now(),
    last_attempt_at  timestamptz,
    finished_at      timestamptz,
    CONSTRAINT webhook_deliveries_once UNIQUE (webhook_id, event_seq),
    CONSTRAINT webhook_deliveries_finished CHECK ((state = 'pending') = (finished_at IS NULL)),
    -- A delivery that went no further says why.
    CONSTRAINT webhook_deliveries_terminal_reason CHECK (
        state NOT IN ('failed', 'skipped') OR last_reason IS NOT NULL)
);
-- A webhook's history, newest first.
CREATE INDEX webhook_deliveries_webhook ON forge.webhook_deliveries (webhook_id, id DESC);
-- The retention and the doctor's recent problems.
CREATE INDEX webhook_deliveries_finished_at ON forge.webhook_deliveries (finished_at)
    WHERE finished_at IS NOT NULL;
CREATE INDEX webhook_deliveries_open ON forge.webhook_deliveries (created_at)
    WHERE state = 'pending';

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.webhooks TO forge_app;
GRANT SELECT (id, uid, project_id, owner_user_id, origin, event_types, enabled, disabled_reason,
    secret_key_id, secret_version, secret_prev_key_id, secret_prev_until, consecutive_failures,
    failing_since, last_delivery_at, created_at, updated_at) ON forge.webhooks TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.webhook_deliveries TO forge_app;
GRANT SELECT ON forge.webhook_deliveries TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.webhooks')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261120200000, "guarded_changes": ["forge.webhooks.audited_change_present"]}');
