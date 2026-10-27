-- Step C2 (ADR-0016 §1, ADR-0018 §1 and §3 with the 2026-10-27 amendment):
-- the event outbox, the audit log, the feed state, subscribers, dead
-- letters and jobs; the append-only guards of events and audit_log; the
-- privileges of every role; the registry rows of the guarded objects.
-- Runs as forge_owner (forge db migrate). Every object is qualified.
-- The DDL-status decision is forge_audit.ddl_audit_status() (bootstrap).
-- Not here (later steps): the feed-state row (the sequencer creates it with
-- forge_audit.cluster_identity(), C4), the deferred audit-presence triggers
-- (with ChangeTx, C3), retention_policy (C7), audit_seal (C8).

-- ── Outbox ─────────────────────────────────────────────────────────────
CREATE TABLE forge.events (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    seq             bigint UNIQUE,
    event_id        uuid NOT NULL UNIQUE,
    event_type      text NOT NULL CHECK (event_type ~ '^[a-z][a-z_]*(\.[a-z][a-z_]*){1,3}$'),
    schema_version  smallint NOT NULL CHECK (schema_version >= 1),
    aggregate_type  text NOT NULL,
    aggregate_id    bigint NOT NULL,
    project_id      bigint,
    actor_kind      text NOT NULL CHECK (actor_kind IN
                        ('user', 'service_account', 'user_via_service_account', 'system')),
    actor_user_id   bigint,
    actor_via_id    bigint,
    CONSTRAINT events_actor_consistent CHECK (
           (actor_kind = 'user'                     AND actor_user_id IS NOT NULL AND actor_via_id IS NULL)
        OR (actor_kind = 'service_account'          AND actor_user_id IS NOT NULL AND actor_via_id IS NULL)
        OR (actor_kind = 'user_via_service_account' AND actor_user_id IS NOT NULL AND actor_via_id IS NOT NULL)
        OR (actor_kind = 'system'                   AND actor_user_id IS NULL     AND actor_via_id IS NULL)),
    request_id      text NOT NULL,
    caused_by_seq   bigint,
    root_seq        bigint,
    causation_depth smallint NOT NULL DEFAULT 0 CHECK (causation_depth BETWEEN 0 AND 8),
    CONSTRAINT events_causation_consistent CHECK (
        (caused_by_seq IS NULL) = (root_seq IS NULL)
        AND (caused_by_seq IS NULL) = (causation_depth = 0)),
    dedup_key       text UNIQUE CHECK (length(dedup_key) <= 200),
    txid            xid8 NOT NULL DEFAULT pg_catalog.pg_current_xact_id(),
    occurred_at     timestamptz NOT NULL DEFAULT pg_catalog.now(),
    sequenced_at    timestamptz,
    payload         jsonb NOT NULL CHECK (pg_catalog.pg_column_size(payload) <= 65536)
) WITH (autovacuum_vacuum_scale_factor = 0.05);
CREATE INDEX events_unsequenced ON forge.events (id) WHERE seq IS NULL;
CREATE INDEX events_project_seq ON forge.events (project_id, seq) WHERE seq IS NOT NULL;
CREATE INDEX events_root ON forge.events (root_seq) WHERE root_seq IS NOT NULL;

CREATE TABLE forge.event_feed_state (
    singleton                boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    epoch                    uuid NOT NULL,
    system_identifier        bigint NOT NULL,
    timeline_id              integer NOT NULL,
    high_seq                 bigint NOT NULL DEFAULT 0,
    retained_from            bigint NOT NULL DEFAULT 1,
    audit_high_seq           bigint NOT NULL DEFAULT 0,
    audit_sequencing_enabled boolean NOT NULL DEFAULT false,
    rollback_handled_seal_id bigint,
    epoch_rotated_at         timestamptz,
    sequencer_heartbeat      timestamptz NOT NULL
);

CREATE TABLE forge.event_subscribers (
    name               text PRIMARY KEY,
    ordering           text NOT NULL CHECK (ordering IN ('ordered', 'independent')),
    enabled            boolean NOT NULL DEFAULT true,
    retention_blocking boolean NOT NULL DEFAULT true,
    last_seq           bigint NOT NULL,
    last_txid          xid8,
    updated_at         timestamptz NOT NULL,
    failures           integer NOT NULL DEFAULT 0,
    failing_since      timestamptz,
    last_error_code    text CHECK (last_error_code ~ '^FG-[A-Z]+-[0-9]{4}$')
);

CREATE TABLE forge.subscriber_dead_letters (
    subscriber     text NOT NULL,
    seq            bigint NOT NULL,
    error_code     text NOT NULL CHECK (error_code ~ '^FG-[A-Z]+-[0-9]{4}$'),
    attempts       integer NOT NULL,
    quarantined_at timestamptz NOT NULL DEFAULT pg_catalog.now(),
    PRIMARY KEY (subscriber, seq)
);

-- ── Jobs ───────────────────────────────────────────────────────────────
CREATE TABLE forge.jobs (
    id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    kind              text NOT NULL,
    payload           jsonb NOT NULL CHECK (pg_catalog.pg_column_size(payload) <= 16384),
    state             text NOT NULL CHECK (state IN ('queued', 'running', 'succeeded', 'dead')),
    attempts          integer NOT NULL DEFAULT 0,
    max_attempts      integer NOT NULL CHECK (max_attempts >= 1),
    run_after         timestamptz NOT NULL DEFAULT pg_catalog.now(),
    lease_until       timestamptz,
    worker_id         text,
    timeout_ms        integer NOT NULL CHECK (timeout_ms BETWEEN 1 AND 3600000),
    dedup_key         text CHECK (length(dedup_key) <= 200),
    origin_request_id text NOT NULL,
    origin_event_seq  bigint,
    last_error_code   text CHECK (last_error_code ~ '^FG-[A-Z]+-[0-9]{4}$'),
    last_error        jsonb CHECK (pg_catalog.pg_column_size(last_error) <= 8192),
    created_at        timestamptz NOT NULL DEFAULT pg_catalog.now(),
    finished_at       timestamptz,
    CONSTRAINT jobs_attempts_bounded CHECK (attempts BETWEEN 0 AND max_attempts),
    CONSTRAINT jobs_running_leased CHECK (state <> 'running'
        OR (lease_until IS NOT NULL AND worker_id IS NOT NULL)),
    CONSTRAINT jobs_dead_has_error CHECK (state <> 'dead' OR last_error_code IS NOT NULL),
    CONSTRAINT jobs_finished_at CHECK (state NOT IN ('succeeded', 'dead') OR finished_at IS NOT NULL)
) WITH (fillfactor = 70, autovacuum_vacuum_scale_factor = 0.02);
CREATE UNIQUE INDEX jobs_dedup ON forge.jobs (kind, dedup_key) WHERE dedup_key IS NOT NULL;
CREATE INDEX jobs_ready ON forge.jobs (run_after) WHERE state = 'queued';
CREATE INDEX jobs_lease ON forge.jobs (lease_until) WHERE state = 'running';
CREATE INDEX jobs_finished ON forge.jobs (finished_at) WHERE state IN ('succeeded', 'dead');

-- ── Audit log ──────────────────────────────────────────────────────────
CREATE TABLE forge.audit_log (
    id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    seq              bigint UNIQUE,
    sequenced_at     timestamptz,
    txid             xid8 NOT NULL DEFAULT pg_catalog.pg_current_xact_id(),
    occurred_at      timestamptz NOT NULL DEFAULT pg_catalog.now(),
    actor_kind       text NOT NULL CHECK (actor_kind IN
                         ('user', 'service_account', 'user_via_service_account', 'system', 'anonymous')),
    actor_user_id    bigint,
    actor_via_id     bigint,
    credential_id    bigint,
    act_as_claimed   text,
    act_as_key_id    smallint,
    via_assertion_id text,
    effective_scope  text[],
    CONSTRAINT audit_actor_consistent CHECK (
           (actor_kind = 'user'                     AND actor_user_id IS NOT NULL AND actor_via_id IS NULL)
        OR (actor_kind = 'service_account'          AND actor_user_id IS NOT NULL AND actor_via_id IS NULL)
        OR (actor_kind = 'user_via_service_account' AND actor_user_id IS NOT NULL AND actor_via_id IS NOT NULL
                                                    AND credential_id IS NOT NULL AND act_as_claimed IS NOT NULL
                                                    AND act_as_key_id IS NOT NULL)
        OR (actor_kind IN ('system', 'anonymous')   AND actor_user_id IS NULL     AND actor_via_id IS NULL)),
    CONSTRAINT audit_act_as_key CHECK ((act_as_claimed IS NULL) = (act_as_key_id IS NULL)),
    actor_label      text CHECK (length(actor_label) <= 64),
    action           text NOT NULL,
    outcome          text NOT NULL CHECK (outcome IN ('succeeded', 'denied', 'failed')),
    resource_type    text NOT NULL,
    resource_id      bigint,
    project_id       bigint,
    source_ip        inet,
    user_agent       text CHECK (length(user_agent) <= 256),
    request_id       text NOT NULL,
    error_code       text CHECK (error_code ~ '^FG-[A-Z]+-[0-9]{4}$'),
    occurrences      integer NOT NULL DEFAULT 1 CHECK (occurrences >= 1),
    details          jsonb NOT NULL DEFAULT '{}' CHECK (pg_catalog.pg_column_size(details) <= 8192)
) WITH (autovacuum_vacuum_scale_factor = 0.05);
CREATE INDEX audit_log_unsequenced ON forge.audit_log (id) WHERE seq IS NULL;
CREATE INDEX audit_log_txid ON forge.audit_log (txid, resource_type, resource_id);
CREATE INDEX audit_log_project_time ON forge.audit_log (project_id, occurred_at);
CREATE INDEX audit_log_actor_time ON forge.audit_log (actor_user_id, occurred_at);
CREATE INDEX audit_log_request ON forge.audit_log (request_id);

-- ── Append-only guards (XF001, ADR-0018 §3) ────────────────────────────
-- One function per table (they read different columns). Not SECURITY
-- DEFINER: current_user is the role doing the change — forge_seq inside the
-- sequencing function, forge_purge inside the retention / redaction ones.
-- search_path holds no writable schema: every name is qualified, and
-- operators resolve in pg_catalog only (an operator planted in forge could
-- otherwise run inside the guard).
CREATE FUNCTION forge.events_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    high bigint;
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.seq IS NOT NULL OR NEW.sequenced_at IS NOT NULL THEN
            RAISE EXCEPTION USING ERRCODE = 'XF001',
                MESSAGE = 'INSERT on forge.events with seq is not allowed (assigned by the sequencer)';
        END IF;
        NEW.txid := pg_catalog.pg_current_xact_id();
        NEW.occurred_at := pg_catalog.now();
        RETURN NEW;
    ELSIF TG_OP = 'UPDATE' THEN
        IF current_user = 'forge_seq' AND OLD.seq IS NULL AND NEW.seq IS NOT NULL
           AND (pg_catalog.to_jsonb(NEW) - 'seq'::text - 'sequenced_at'::text)
               = (pg_catalog.to_jsonb(OLD) - 'seq'::text - 'sequenced_at'::text) THEN
            SELECT s.high_seq INTO high FROM forge.event_feed_state AS s WHERE s.singleton;
            IF high IS NULL OR NEW.seq <= high OR NEW.seq > high + 1000 THEN
                RAISE EXCEPTION USING ERRCODE = 'XF001',
                    MESSAGE = pg_catalog.format(
                        'seq %s on forge.events is outside the sequencing window (high_seq %s)',
                        NEW.seq, coalesce(high::text, 'missing'));
            END IF;
            NEW.sequenced_at := pg_catalog.now();
            RETURN NEW;
        ELSIF current_user = 'forge_purge'
              AND (pg_catalog.to_jsonb(NEW) - 'payload'::text)
                  = (pg_catalog.to_jsonb(OLD) - 'payload'::text) THEN
            RETURN NEW;
        END IF;
    ELSIF TG_OP = 'DELETE' THEN
        -- Retention only: sequenced (published) rows past the 7-day floor.
        IF current_user = 'forge_purge' AND OLD.seq IS NOT NULL
           AND OLD.occurred_at < pg_catalog.now() - interval '7 days' THEN
            RETURN OLD;
        END IF;
    END IF;
    RAISE EXCEPTION USING ERRCODE = 'XF001',
        MESSAGE = pg_catalog.format('%s on forge.events is not allowed (append-only)', TG_OP);
END
$$;
REVOKE ALL ON FUNCTION forge.events_guard() FROM PUBLIC;

CREATE FUNCTION forge.audit_log_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    high bigint;
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.seq IS NOT NULL OR NEW.sequenced_at IS NOT NULL THEN
            RAISE EXCEPTION USING ERRCODE = 'XF001',
                MESSAGE = 'INSERT on forge.audit_log with seq is not allowed (assigned by the sequencer)';
        END IF;
        -- The application never writes the rows that the migrations and the
        -- privileged functions write about themselves.
        IF current_user = 'forge_app'
           AND (NEW.action = 'db.migrate' OR NEW.action LIKE 'audit.%'
                OR NEW.action LIKE 'feed.%' OR NEW.action LIKE 'events.%') THEN
            RAISE EXCEPTION USING ERRCODE = 'XF001',
                MESSAGE = pg_catalog.format('audit action %s is reserved', NEW.action);
        END IF;
        NEW.txid := pg_catalog.pg_current_xact_id();
        NEW.occurred_at := pg_catalog.now();
        RETURN NEW;
    ELSIF TG_OP = 'UPDATE' THEN
        IF current_user = 'forge_seq' AND OLD.seq IS NULL AND NEW.seq IS NOT NULL
           AND (pg_catalog.to_jsonb(NEW) - 'seq'::text - 'sequenced_at'::text)
               = (pg_catalog.to_jsonb(OLD) - 'seq'::text - 'sequenced_at'::text) THEN
            SELECT s.audit_high_seq INTO high FROM forge.event_feed_state AS s WHERE s.singleton;
            IF high IS NULL OR NEW.seq <= high OR NEW.seq > high + 1000 THEN
                RAISE EXCEPTION USING ERRCODE = 'XF001',
                    MESSAGE = pg_catalog.format(
                        'seq %s on forge.audit_log is outside the sequencing window (audit_high_seq %s)',
                        NEW.seq, coalesce(high::text, 'missing'));
            END IF;
            NEW.sequenced_at := pg_catalog.now();
            RETURN NEW;
        -- Redaction: all personal columns to NULL at once, nothing else (the
        -- per-seal-version check comes with the seals, C8).
        ELSIF current_user = 'forge_purge'
              AND NEW.actor_label IS NULL AND NEW.source_ip IS NULL AND NEW.user_agent IS NULL
              AND (pg_catalog.to_jsonb(NEW) - 'actor_label'::text - 'source_ip'::text
                   - 'user_agent'::text)
                  = (pg_catalog.to_jsonb(OLD) - 'actor_label'::text - 'source_ip'::text
                     - 'user_agent'::text) THEN
            RETURN NEW;
        END IF;
    ELSIF TG_OP = 'DELETE' THEN
        -- Purge only: sequenced rows past the 365-day floor, never the
        -- migration records (forge_audit.ddl_audit_status() matches DDL to
        -- them).
        IF current_user = 'forge_purge' AND OLD.seq IS NOT NULL
           AND OLD.action <> 'db.migrate'
           AND OLD.occurred_at < pg_catalog.now() - interval '365 days' THEN
            RETURN OLD;
        END IF;
    END IF;
    RAISE EXCEPTION USING ERRCODE = 'XF001',
        MESSAGE = pg_catalog.format('%s on forge.audit_log is not allowed (append-only)', TG_OP);
END
$$;
REVOKE ALL ON FUNCTION forge.audit_log_guard() FROM PUBLIC;

-- The feed state: one row; the counters only grow (a sequencer bug or a
-- misused credential cannot move them back and renumber behind readers);
-- retained_from stays within [1, high_seq + 1]; the identity columns change
-- only through forge_seq (rotate_epoch, C4); the row is never deleted.
CREATE FUNCTION forge.event_feed_state_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP = 'UPDATE'
       AND NEW.high_seq >= OLD.high_seq
       AND NEW.audit_high_seq >= OLD.audit_high_seq
       AND NEW.retained_from >= OLD.retained_from
       AND NEW.retained_from <= NEW.high_seq + 1
       AND (current_user = 'forge_seq'
            OR (NEW.epoch, NEW.system_identifier, NEW.timeline_id)
               IS NOT DISTINCT FROM (OLD.epoch, OLD.system_identifier, OLD.timeline_id)) THEN
        RETURN NEW;
    END IF;
    RAISE EXCEPTION USING ERRCODE = 'XF001',
        MESSAGE = pg_catalog.format('%s on forge.event_feed_state is not allowed', TG_OP);
END
$$;
REVOKE ALL ON FUNCTION forge.event_feed_state_guard() FROM PUBLIC;

CREATE TRIGGER events_append_only BEFORE INSERT OR UPDATE OR DELETE ON forge.events
    FOR EACH ROW EXECUTE FUNCTION forge.events_guard();
CREATE TRIGGER events_no_truncate BEFORE TRUNCATE ON forge.events
    FOR EACH STATEMENT EXECUTE FUNCTION forge.events_guard();
CREATE TRIGGER audit_log_append_only BEFORE INSERT OR UPDATE OR DELETE ON forge.audit_log
    FOR EACH ROW EXECUTE FUNCTION forge.audit_log_guard();
CREATE TRIGGER audit_log_no_truncate BEFORE TRUNCATE ON forge.audit_log
    FOR EACH STATEMENT EXECUTE FUNCTION forge.audit_log_guard();
CREATE TRIGGER event_feed_state_append_only BEFORE UPDATE OR DELETE ON forge.event_feed_state
    FOR EACH ROW EXECUTE FUNCTION forge.event_feed_state_guard();
CREATE TRIGGER event_feed_state_no_truncate BEFORE TRUNCATE ON forge.event_feed_state
    FOR EACH STATEMENT EXECUTE FUNCTION forge.event_feed_state_guard();
-- ALWAYS: also under session_replication_role = replica.
ALTER TABLE forge.events ENABLE ALWAYS TRIGGER events_append_only;
ALTER TABLE forge.events ENABLE ALWAYS TRIGGER events_no_truncate;
ALTER TABLE forge.audit_log ENABLE ALWAYS TRIGGER audit_log_append_only;
ALTER TABLE forge.audit_log ENABLE ALWAYS TRIGGER audit_log_no_truncate;
ALTER TABLE forge.event_feed_state ENABLE ALWAYS TRIGGER event_feed_state_append_only;
ALTER TABLE forge.event_feed_state ENABLE ALWAYS TRIGGER event_feed_state_no_truncate;

-- ── Privileges (ADR-0018 §1) ───────────────────────────────────────────
-- forge_app writes events and audit rows but never id, seq, sequenced_at,
-- txid or occurred_at (defaults and the guard decide them), and never
-- updates or deletes them.
GRANT SELECT ON forge.events, forge.audit_log, forge.event_feed_state TO forge_app;
GRANT INSERT (event_id, event_type, schema_version, aggregate_type, aggregate_id, project_id,
    actor_kind, actor_user_id, actor_via_id, request_id, caused_by_seq, root_seq,
    causation_depth, dedup_key, payload) ON forge.events TO forge_app;
GRANT INSERT (actor_kind, actor_user_id, actor_via_id, credential_id, act_as_claimed,
    act_as_key_id, via_assertion_id, effective_scope, actor_label, action, outcome,
    resource_type, resource_id, project_id, source_ip, user_agent, request_id, error_code,
    occurrences, details) ON forge.audit_log TO forge_app, forge_seq, forge_purge;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.jobs TO forge_app;
-- Subscribers: the application advances its own progress; whether a
-- subscriber is enabled or holds back retention is not its decision (a
-- leaked credential could otherwise unblock retention past a lagging one).
GRANT SELECT, INSERT (name, ordering, last_seq, last_txid, updated_at),
    UPDATE (last_seq, last_txid, updated_at, failures, failing_since, last_error_code)
    ON forge.event_subscribers TO forge_app;
-- Quarantined events: recorded by the application, removed only by
-- retention (forge_purge).
GRANT SELECT, INSERT ON forge.subscriber_dead_letters TO forge_app;

-- The sequencer (functions owned by forge_seq, C4).
GRANT SELECT, UPDATE (seq, sequenced_at) ON forge.events, forge.audit_log TO forge_seq;
GRANT SELECT, INSERT, UPDATE ON forge.event_feed_state TO forge_seq;

-- Retention and redaction (functions owned by forge_purge, C7 / C10).
GRANT SELECT, DELETE ON forge.events, forge.audit_log TO forge_purge;
GRANT UPDATE (payload) ON forge.events TO forge_purge;
GRANT UPDATE (actor_label, source_ip, user_agent) ON forge.audit_log TO forge_purge;
GRANT SELECT, UPDATE (retained_from) ON forge.event_feed_state TO forge_purge;
GRANT SELECT ON forge.event_subscribers TO forge_purge;
GRANT SELECT, DELETE ON forge.subscriber_dead_letters TO forge_purge;

-- The read pool: the ledger's ro_columns (no personal or act-as columns of
-- the audit log).
GRANT SELECT ON forge.events, forge.event_feed_state, forge.event_subscribers,
    forge.subscriber_dead_letters, forge.jobs TO forge_ro;
GRANT SELECT (id, seq, sequenced_at, txid, occurred_at, actor_kind, actor_user_id,
    actor_via_id, credential_id, via_assertion_id, effective_scope, action, outcome,
    resource_type, resource_id, project_id, request_id, error_code, occurrences, details)
    ON forge.audit_log TO forge_ro;
-- The maintenance login reads what forge_audit.ddl_audit_status() needs to
-- list DDL for acknowledgement.
GRANT SELECT (txid, action, details) ON forge.audit_log TO forge_maint;

-- ── Guarded objects (ADR-0018 §3) ──────────────────────────────────────
INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('table', 'forge.events'),
    ('table', 'forge.audit_log'),
    ('table', 'forge.event_feed_state'),
    ('function', 'forge.events_guard()'),
    ('function', 'forge.audit_log_guard()'),
    ('function', 'forge.event_feed_state_guard()'),
    ('trigger', 'events_append_only on forge.events'),
    ('trigger', 'events_no_truncate on forge.events'),
    ('trigger', 'audit_log_append_only on forge.audit_log'),
    ('trigger', 'audit_log_no_truncate on forge.audit_log'),
    ('trigger', 'event_feed_state_append_only on forge.event_feed_state'),
    ('trigger', 'event_feed_state_no_truncate on forge.event_feed_state')
ON CONFLICT DO NOTHING;

-- Last: this migration's own audit row (same transaction as the DDL above,
-- so forge_audit.ddl_audit_status() can match it by txid).
INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261027100000, "guarded_changes": ["forge.events", "forge.audit_log", "forge.event_feed_state", "forge.events_guard", "forge.audit_log_guard", "forge.event_feed_state_guard", "forge.events.events_append_only", "forge.events.events_no_truncate", "forge.audit_log.audit_log_append_only", "forge.audit_log.audit_log_no_truncate", "forge.event_feed_state.event_feed_state_append_only", "forge.event_feed_state.event_feed_state_no_truncate"]}');
