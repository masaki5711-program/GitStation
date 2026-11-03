-- M3 I1c (plan 02 §5.7 / §11.2, ADR-0016 §2, §7 and its Amendment I1c):
-- the event feed of an integration.
--
-- 1. forge.integration_bindings.generation: a new number for every bind
--    (an identity column: forge_app cannot choose it). The feed's visible
--    set — and so every cursor — includes it, so an unbind followed by a
--    rebind of the same project answers 410 visible_set_changed (I1b ends a
--    project's links on unbind without work_item.unlinked events; the
--    consumer must resync). Existing bindings get theirs here. The audit
--    trigger is re-created with the column in UPDATE OF (README); the
--    column never changes (forge_app updates granted_membership only).
-- 2. forge.integration_feed_reads (derived): the last read of each
--    integration's feed — written at most once a minute per integration —
--    for the lag check (doctor integrations.feed_lag, FG-INT-0009).
-- 3. forge.retention_policy: one row, events_days (default 90, at least 7;
--    ADR-0016 §2). forge_app cannot write it: a leaked application
--    credential must not shorten retention. Changed by a migration.
-- 4. forge.events_retention(max_rows, request_id), owned by forge_purge
--    (ADR-0016 §2, ADR-0018 §3): one bounded batch. From retained_from it
--    deletes the contiguous run of events (and their event_work_items)
--    older than events_days, held back by the enabled retention_blocking
--    subscribers except for rows older than 1.5 × events_days, moves
--    retained_from past the batch and writes one events.retention audit
--    row. Nothing when there is nothing to delete. EXECUTE: forge_app (the
--    worker's events.retention task).
-- 5. forge.feed_new_epoch(reason, request_id), owned by forge_seq
--    (ADR-0016 §7, ADR-0018 §5): `forge admin events new-epoch` after a
--    restore or rebuild in the same cluster. Under the sequencer's lock it
--    gives the feed a new epoch (every cursor then answers 410
--    epoch_mismatch) and writes one feed.epoch_rotated audit row naming
--    both epochs. EXECUTE: forge_maint only (an approved runbook step).
-- 6. tokens_scopes_known gains `user_employee_number` (plan 11 Q7: the
--    feed's actor.employee_number, shaped like user_email).

-- ── 1. binding generations ─────────────────────────────────────────────
ALTER TABLE forge.integration_bindings
    ADD COLUMN generation bigint GENERATED ALWAYS AS IDENTITY;

DROP TRIGGER audited_change_present ON forge.integration_bindings;
CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF integration_id, project_id, external_project_id,
        granted_membership, created_by, created_at, generation
    ON forge.integration_bindings
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.integration_bindings ENABLE ALWAYS TRIGGER audited_change_present;

-- ── 2. the feed's reads ────────────────────────────────────────────────
CREATE TABLE forge.integration_feed_reads (
    integration_id    bigint PRIMARY KEY REFERENCES forge.integrations (id),
    last_read_at      timestamptz NOT NULL,
    -- The position the read answered (its next_cursor).
    cursor_seq        bigint NOT NULL
        CONSTRAINT integration_feed_reads_cursor_seq CHECK (cursor_seq >= 0),
    -- The read's upper bound: min(high_seq, work-item-sync's last_seq).
    read_bound_seq    bigint NOT NULL
        CONSTRAINT integration_feed_reads_bound CHECK (read_bound_seq >= cursor_seq),
    -- When the first visible event after cursor_seq happened (NULL: the
    -- consumer had read everything visible).
    oldest_unread_at  timestamptz
);
GRANT SELECT, INSERT, UPDATE ON forge.integration_feed_reads TO forge_app;
GRANT SELECT ON forge.integration_feed_reads TO forge_ro;

-- ── 3. the retention policy ────────────────────────────────────────────
CREATE TABLE forge.retention_policy (
    singleton    boolean PRIMARY KEY DEFAULT true
        CONSTRAINT retention_policy_singleton CHECK (singleton),
    events_days  integer NOT NULL
        CONSTRAINT retention_policy_events_days CHECK (events_days BETWEEN 7 AND 3650)
);
INSERT INTO forge.retention_policy (singleton, events_days) VALUES (true, 90);
GRANT SELECT ON forge.retention_policy TO forge_app, forge_ro, forge_purge;

-- ── 4. retention ───────────────────────────────────────────────────────
GRANT SELECT, DELETE ON forge.event_work_items TO forge_purge;

CREATE FUNCTION forge.events_retention(max_rows integer, request_id text,
    OUT deleted bigint, OUT from_seq bigint, OUT to_seq bigint)
    LANGUAGE plpgsql
    SECURITY DEFINER
    SET search_path = pg_catalog, forge, pg_temp
AS $$
DECLARE
    v_from bigint;
    v_high bigint;
    v_days integer;
    v_cutoff timestamptz;
    v_hard timestamptz;
    v_block bigint;
    v_stop bigint;
    v_last bigint;
    v_hard_stop bigint;
BEGIN
    IF max_rows IS NULL OR max_rows < 1 OR max_rows > 10000
       OR request_id IS NULL OR length(request_id) NOT BETWEEN 1 AND 128 THEN
        RAISE EXCEPTION USING ERRCODE = '22023',
            MESSAGE = 'forge.events_retention: max_rows must be 1..10000 and request_id 1..128 characters';
    END IF;
    deleted := 0;
    -- The sequencer locks the same row every 200 ms; a batch is short.
    SELECT s.retained_from, s.high_seq INTO v_from, v_high
      FROM ONLY forge.event_feed_state AS s
     WHERE s.singleton
       FOR UPDATE;
    IF NOT FOUND THEN
        RETURN;
    END IF;
    SELECT p.events_days INTO v_days FROM ONLY forge.retention_policy AS p WHERE p.singleton;
    IF v_days IS NULL THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'forge.events_retention: the forge.retention_policy row is missing';
    END IF;
    v_cutoff := pg_catalog.now() - pg_catalog.make_interval(days => v_days);
    v_hard := pg_catalog.now() - pg_catalog.make_interval(days => (v_days * 3) / 2);
    SELECT pg_catalog.min(sub.last_seq) + 1 INTO v_block
      FROM ONLY forge.event_subscribers AS sub
     WHERE sub.enabled AND sub.retention_blocking;
    -- The contiguous run from retained_from past the cutoff, in one
    -- bounded window (no occurred_at index: seq order is time order).
    SELECT pg_catalog.min(e.seq) INTO v_stop
      FROM ONLY forge.events AS e
     WHERE e.seq >= v_from AND e.seq < v_from + max_rows AND e.occurred_at >= v_cutoff;
    v_last := LEAST(coalesce(v_stop, v_from + max_rows) - 1, v_high);
    -- A subscriber that has not handled them keeps them, until the hard
    -- limit (1.5 × the period; it then stops with retention_overrun).
    IF v_block IS NOT NULL AND v_last >= v_block THEN
        SELECT pg_catalog.min(e.seq) INTO v_hard_stop
          FROM ONLY forge.events AS e
         WHERE e.seq >= v_block AND e.seq <= v_last AND e.occurred_at >= v_hard;
        v_last := GREATEST(v_block - 1, coalesce(v_hard_stop, v_last + 1) - 1);
    END IF;
    IF v_last < v_from THEN
        RETURN;
    END IF;
    DELETE FROM ONLY forge.event_work_items AS w WHERE w.seq >= v_from AND w.seq <= v_last;
    DELETE FROM ONLY forge.events AS e WHERE e.seq >= v_from AND e.seq <= v_last;
    GET DIAGNOSTICS deleted = ROW_COUNT;
    UPDATE ONLY forge.event_feed_state AS s SET retained_from = v_last + 1 WHERE s.singleton;
    from_seq := v_from;
    to_seq := v_last;
    INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
    VALUES ('system', 'events.retention', 'succeeded', 'events', request_id,
        pg_catalog.jsonb_build_object('from_seq', v_from, 'to_seq', v_last,
            'deleted', deleted, 'events_days', v_days));
END
$$;
-- Grants before the owner change (as forge.assign_seq).
REVOKE ALL ON FUNCTION forge.events_retention(integer, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION forge.events_retention(integer, text) TO forge_app;
ALTER FUNCTION forge.events_retention(integer, text) OWNER TO forge_purge;

-- ── 5. a new epoch ─────────────────────────────────────────────────────
CREATE FUNCTION forge.feed_new_epoch(reason text, request_id text,
    OUT old_epoch uuid, OUT new_epoch uuid)
    LANGUAGE plpgsql
    SECURITY DEFINER
    SET search_path = pg_catalog, forge, pg_temp
AS $$
BEGIN
    IF reason IS NULL OR reason <> 'manual'
       OR request_id IS NULL OR length(request_id) NOT BETWEEN 1 AND 128 THEN
        RAISE EXCEPTION USING ERRCODE = '22023',
            MESSAGE = 'forge.feed_new_epoch: reason must be manual and request_id 1..128 characters';
    END IF;
    -- The sequencer's lock: no batch is numbered under the old epoch while
    -- the new one is written.
    PERFORM pg_catalog.pg_advisory_xact_lock(1718579815, 1);
    SELECT s.epoch INTO old_epoch
      FROM ONLY forge.event_feed_state AS s
     WHERE s.singleton
       FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION USING ERRCODE = 'P0002',
            MESSAGE = 'forge.feed_new_epoch: no feed state yet (the sequencer creates it)';
    END IF;
    new_epoch := pg_catalog.gen_random_uuid();
    UPDATE ONLY forge.event_feed_state AS s
       SET epoch = new_epoch, epoch_rotated_at = pg_catalog.now()
     WHERE s.singleton;
    INSERT INTO forge.audit_log (actor_kind, actor_label, action, outcome, resource_type,
        request_id, details)
    VALUES ('system', left(session_user::text, 64), 'feed.epoch_rotated', 'succeeded',
        'event_feed', request_id,
        pg_catalog.jsonb_build_object('reason', reason, 'old_epoch', old_epoch,
            'new_epoch', new_epoch));
END
$$;
REVOKE ALL ON FUNCTION forge.feed_new_epoch(text, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION forge.feed_new_epoch(text, text) TO forge_maint;
ALTER FUNCTION forge.feed_new_epoch(text, text) OWNER TO forge_seq;

-- ── 6. the employee number scope ───────────────────────────────────────
ALTER TABLE forge.tokens DROP CONSTRAINT tokens_scopes_known;
ALTER TABLE forge.tokens ADD CONSTRAINT tokens_scopes_known CHECK (
    cardinality(scopes) >= 1
    AND scopes <@ ARRAY['read_repository', 'write_repository', 'read_api', 'api', 'admin',
        'merge_execute', 'merge_approve', 'user_email', 'user_employee_number']::text[]);

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('function', 'forge.events_retention(integer,text)'),
    ('function', 'forge.feed_new_epoch(text,text)')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261117100000, "guarded_changes": ["forge.integration_bindings.audited_change_present", "forge.events_retention", "forge.feed_new_epoch"]}');
