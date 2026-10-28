-- Step C4, review: an idle sequencer writes nothing. The worker calls
-- forge.assign_seq every 200 ms; when no row waits (events, and audit rows
-- when they would be numbered) and the heartbeat is younger than 5 s, the
-- function returns without locking or updating the feed-state row. The
-- heartbeat is therefore at most 5 s stale while the sequencer runs (the
-- lag rule allows 30 s). Replaces forge.assign_seq; the rest is unchanged.
--
-- The function belongs to forge_seq (ADR-0018 §1): only its owner may replace
-- it, so the file acts as forge_seq for the replacement and returns to
-- forge_owner before the db.migrate row (written by the owner only).

SET LOCAL ROLE forge_seq;

CREATE OR REPLACE FUNCTION forge.assign_seq(max_batch integer, include_audit boolean,
    OUT from_seq bigint, OUT to_seq bigint, OUT audit_from bigint, OUT audit_to bigint)
    LANGUAGE plpgsql
    SECURITY DEFINER
    SET search_path = pg_catalog, forge, pg_temp
AS $$
DECLARE
    v_high bigint;
    v_audit_high bigint;
    v_audit_enabled boolean;
    v_sysid bigint;
    v_timeline integer;
    v_count bigint;
    v_first bigint;
    v_last bigint;
    v_audit_count bigint := 0;
    v_beat timestamptz;
BEGIN
    IF max_batch IS NULL OR max_batch < 1 OR max_batch > 1000 OR include_audit IS NULL THEN
        RAISE EXCEPTION USING ERRCODE = '22023',
            MESSAGE = 'forge.assign_seq: max_batch must be 1..1000 and include_audit not null';
    END IF;
    -- One sequencer at a time; a second caller waits (lock_timeout bounds it).
    PERFORM pg_catalog.pg_advisory_xact_lock(1718579815, 1);
    -- Idle: nothing waits and the heartbeat is fresh — return without
    -- locking or writing the feed-state row.
    SELECT s.sequencer_heartbeat, s.audit_sequencing_enabled INTO v_beat, v_audit_enabled
      FROM ONLY forge.event_feed_state AS s
     WHERE s.singleton;
    IF FOUND AND v_beat > pg_catalog.now() - interval '5 seconds'
       AND NOT EXISTS (SELECT 1 FROM ONLY forge.events AS e WHERE e.seq IS NULL)
       AND NOT (include_audit AND v_audit_enabled
                AND EXISTS (SELECT 1 FROM ONLY forge.audit_log AS a WHERE a.seq IS NULL)) THEN
        RETURN;
    END IF;
    SELECT s.high_seq, s.audit_high_seq, s.audit_sequencing_enabled
      INTO v_high, v_audit_high, v_audit_enabled
      FROM ONLY forge.event_feed_state AS s
     WHERE s.singleton
       FOR UPDATE;
    IF NOT FOUND THEN
        SELECT c.system_identifier, c.timeline_id INTO v_sysid, v_timeline
          FROM forge_audit.cluster_identity() AS c;
        INSERT INTO forge.event_feed_state
            (epoch, system_identifier, timeline_id, sequencer_heartbeat)
        VALUES (pg_catalog.gen_random_uuid(), v_sysid, v_timeline, pg_catalog.now())
        RETURNING high_seq, audit_high_seq, audit_sequencing_enabled
             INTO v_high, v_audit_high, v_audit_enabled;
    END IF;

    WITH picked AS (
        SELECT e.id FROM ONLY forge.events AS e
         WHERE e.seq IS NULL
         ORDER BY e.id
         LIMIT max_batch
           FOR UPDATE
    ), numbered AS (
        SELECT p.id, v_high + pg_catalog.row_number() OVER (ORDER BY p.id) AS new_seq
          FROM picked AS p
    ), done AS (
        UPDATE ONLY forge.events AS e
           SET seq = nb.new_seq
          FROM numbered AS nb
         WHERE e.id = nb.id
        RETURNING e.seq
    )
    SELECT pg_catalog.count(*), pg_catalog.min(d.seq), pg_catalog.max(d.seq)
      INTO v_count, v_first, v_last
      FROM done AS d;
    IF v_count > 0 THEN
        IF v_first <> v_high + 1 OR v_last - v_first + 1 <> v_count THEN
            RAISE EXCEPTION USING ERRCODE = 'XF001',
                MESSAGE = pg_catalog.format(
                    'forge.assign_seq: events %s..%s for %s rows after high_seq %s',
                    v_first, v_last, v_count, v_high);
        END IF;
        from_seq := v_first;
        to_seq := v_last;
    END IF;

    IF include_audit AND v_audit_enabled THEN
        WITH picked AS (
            SELECT a.id FROM ONLY forge.audit_log AS a
             WHERE a.seq IS NULL
             ORDER BY a.id
             LIMIT max_batch
               FOR UPDATE
        ), numbered AS (
            SELECT p.id, v_audit_high + pg_catalog.row_number() OVER (ORDER BY p.id) AS new_seq
              FROM picked AS p
        ), done AS (
            UPDATE ONLY forge.audit_log AS a
               SET seq = nb.new_seq
              FROM numbered AS nb
             WHERE a.id = nb.id
            RETURNING a.seq
        )
        SELECT pg_catalog.count(*), pg_catalog.min(d.seq), pg_catalog.max(d.seq)
          INTO v_audit_count, v_first, v_last
          FROM done AS d;
        IF v_audit_count > 0 THEN
            IF v_first <> v_audit_high + 1 OR v_last - v_first + 1 <> v_audit_count THEN
                RAISE EXCEPTION USING ERRCODE = 'XF001',
                    MESSAGE = pg_catalog.format(
                        'forge.assign_seq: audit rows %s..%s for %s rows after audit_high_seq %s',
                        v_first, v_last, v_audit_count, v_audit_high);
            END IF;
            audit_from := v_first;
            audit_to := v_last;
        END IF;
    END IF;

    UPDATE ONLY forge.event_feed_state AS s
       SET high_seq = v_high + v_count,
           audit_high_seq = v_audit_high + v_audit_count,
           sequencer_heartbeat = pg_catalog.now()
     WHERE s.singleton;
END
$$;
REVOKE ALL ON FUNCTION forge.assign_seq(integer, boolean) FROM PUBLIC;
SET LOCAL ROLE forge_owner;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261027110100, "guarded_changes": ["forge.assign_seq"]}');
