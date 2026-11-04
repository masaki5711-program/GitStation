-- M3 I1c review (2026-11-04; ADR-0016 Amendment I1c, "I1c の独立レビュー"):
--
-- 1. forge_app inserts bindings by column, never `generation`: with the
--    table-level INSERT it could write `OVERRIDING SYSTEM VALUE` and reuse an
--    old generation, so a cursor of the earlier binding would continue
--    across an unbind and a rebind (database MEDIUM). The bind command
--    names exactly these columns; created_at keeps its default.
-- 2. forge.events_retention (still owned by forge_purge, same signature):
--    the feed state row is no longer locked for the whole batch — the state
--    is read without a lock, the rows go first, and the row is locked only
--    by the final UPDATE (retained_from = GREATEST(...), monotonic even if
--    two batches ran at once), so the sequencer and forge.feed_new_epoch
--    wait at most for that statement (database WARN). The audit row names
--    the retention-blocking subscribers the 1.5 x limit overrode, with
--    their last_seq (LOW). The caller's batch is smaller too (forge-db
--    RETENTION_BATCH = 2 000).

-- ── 1. column-level INSERT on bindings ─────────────────────────────────
REVOKE INSERT ON forge.integration_bindings FROM forge_app;
GRANT INSERT (integration_id, project_id, external_project_id, granted_membership, created_by)
    ON forge.integration_bindings TO forge_app;

-- ── 2. retention without holding the feed state ────────────────────────
SET LOCAL ROLE forge_purge;
CREATE OR REPLACE FUNCTION forge.events_retention(max_rows integer, request_id text,
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
    v_overridden jsonb := '[]'::jsonb;
BEGIN
    IF max_rows IS NULL OR max_rows < 1 OR max_rows > 10000
       OR request_id IS NULL OR length(request_id) NOT BETWEEN 1 AND 128 THEN
        RAISE EXCEPTION USING ERRCODE = '22023',
            MESSAGE = 'forge.events_retention: max_rows must be 1..10000 and request_id 1..128 characters';
    END IF;
    deleted := 0;
    -- No lock here: the sequencer locks this row every 200 ms.
    SELECT s.retained_from, s.high_seq INTO v_from, v_high
      FROM ONLY forge.event_feed_state AS s
     WHERE s.singleton;
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
    SELECT pg_catalog.min(e.seq) INTO v_stop
      FROM ONLY forge.events AS e
     WHERE e.seq >= v_from AND e.seq < v_from + max_rows AND e.occurred_at >= v_cutoff;
    v_last := LEAST(coalesce(v_stop, v_from + max_rows) - 1, v_high);
    IF v_block IS NOT NULL AND v_last >= v_block THEN
        SELECT pg_catalog.min(e.seq) INTO v_hard_stop
          FROM ONLY forge.events AS e
         WHERE e.seq >= v_block AND e.seq <= v_last AND e.occurred_at >= v_hard;
        v_last := GREATEST(v_block - 1, coalesce(v_hard_stop, v_last + 1) - 1);
        IF v_last >= v_block THEN
            SELECT coalesce(pg_catalog.jsonb_agg(pg_catalog.jsonb_build_object(
                       'subscriber', sub.name, 'last_seq', sub.last_seq) ORDER BY sub.name),
                   '[]'::jsonb)
              INTO v_overridden
              FROM ONLY forge.event_subscribers AS sub
             WHERE sub.enabled AND sub.retention_blocking AND sub.last_seq < v_last;
        END IF;
    END IF;
    IF v_last < v_from THEN
        RETURN;
    END IF;
    DELETE FROM ONLY forge.event_work_items AS w WHERE w.seq >= v_from AND w.seq <= v_last;
    DELETE FROM ONLY forge.events AS e WHERE e.seq >= v_from AND e.seq <= v_last;
    GET DIAGNOSTICS deleted = ROW_COUNT;
    -- The row lock only now, for the last statement before COMMIT.
    UPDATE ONLY forge.event_feed_state AS s
       SET retained_from = GREATEST(s.retained_from, v_last + 1)
     WHERE s.singleton;
    from_seq := v_from;
    to_seq := v_last;
    INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
    VALUES ('system', 'events.retention', 'succeeded', 'events', request_id,
        pg_catalog.jsonb_build_object('from_seq', v_from, 'to_seq', v_last,
            'deleted', deleted, 'events_days', v_days, 'overridden', v_overridden));
END
$$;
REVOKE ALL ON FUNCTION forge.events_retention(integer, text) FROM PUBLIC;
SET LOCAL ROLE forge_owner;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261118100100, "guarded_changes": ["forge.events_retention"]}');
