-- Step C5 (ADR-0016 §1 "派生イベントの検査", §6 DerivedTx, §7 購読者, and the
-- C3 amendment): derived events — written by a subscriber (forge-db
-- DerivedTx) while it processes one published event — need no audit row of
-- their own, but must carry a consistent lineage and commit together with
-- the subscriber's progress past their origin.
--
-- - events_lineage (BEFORE INSERT, derived rows only): the origin is a
--   published event (seq set), it is not already at the depth limit (8),
--   root_seq = coalesce(origin.root_seq, origin.seq),
--   causation_depth = origin.causation_depth + 1, and the root has fewer
--   than 100 derived events (fan-out limit). XF002 otherwise.
-- - events_derived_progress (deferred AFTER INSERT, derived rows only): a
--   subscriber advanced last_seq to caused_by_seq in this transaction
--   (last_txid = pg_current_xact_id()). XF002 at COMMIT otherwise. This
--   relies on one transaction processing exactly one origin event.
-- - events_audit_present gets back its WHEN (NEW.caused_by_seq IS NULL):
--   plain events still need an audit row in their transaction.
--
-- XF002 carries CONSTRAINT = derived_lineage / derived_depth /
-- derived_fanout / derived_progress (the FG-DB-0013 reason). The checks are
-- accident-level (ADR-0016 §1): the application credentials can also write
-- subscriber rows. The fan-out count is per statement: two transactions
-- deriving from the same root at once may together pass 100 slightly.
-- Trigger functions are owned by forge_owner like the other guards (not
-- SECURITY DEFINER: they run as the inserting role, which reads events and
-- event_subscribers).

CREATE FUNCTION forge.events_lineage() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    v_seq bigint;
    v_root bigint;
    v_depth smallint;
    v_derived bigint;
BEGIN
    SELECT o.seq, o.root_seq, o.causation_depth INTO v_seq, v_root, v_depth
      FROM ONLY forge.events AS o
     WHERE o.seq = NEW.caused_by_seq;
    IF NOT FOUND THEN
        RAISE EXCEPTION USING ERRCODE = 'XF002', CONSTRAINT = 'derived_lineage',
            MESSAGE = pg_catalog.format(
                'derived event: its origin seq %s is not a published event', NEW.caused_by_seq);
    END IF;
    IF v_depth >= 8 THEN
        RAISE EXCEPTION USING ERRCODE = 'XF002', CONSTRAINT = 'derived_depth',
            MESSAGE = pg_catalog.format(
                'derived event: its origin seq %s is at the causation depth limit 8', v_seq);
    END IF;
    IF NEW.root_seq IS DISTINCT FROM coalesce(v_root, v_seq)
       OR NEW.causation_depth IS DISTINCT FROM v_depth + 1 THEN
        RAISE EXCEPTION USING ERRCODE = 'XF002', CONSTRAINT = 'derived_lineage',
            MESSAGE = pg_catalog.format(
                'derived event: root_seq / causation_depth do not follow origin seq %s', v_seq);
    END IF;
    SELECT pg_catalog.count(*) INTO v_derived
      FROM (SELECT 1 FROM ONLY forge.events AS d WHERE d.root_seq = NEW.root_seq LIMIT 100) AS w;
    IF v_derived >= 100 THEN
        RAISE EXCEPTION USING ERRCODE = 'XF002', CONSTRAINT = 'derived_fanout',
            MESSAGE = pg_catalog.format(
                'derived event: root seq %s already has 100 derived events', NEW.root_seq);
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.events_lineage() FROM PUBLIC;

CREATE FUNCTION forge.events_derived_progress() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM ONLY forge.event_subscribers AS s
         WHERE s.last_txid = pg_catalog.pg_current_xact_id()
           AND s.last_seq = NEW.caused_by_seq
    ) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF002', CONSTRAINT = 'derived_progress',
            MESSAGE = pg_catalog.format(
                'derived event: no subscriber advanced to its origin seq %s in its transaction',
                NEW.caused_by_seq);
    END IF;
    RETURN NULL;
END
$$;
REVOKE ALL ON FUNCTION forge.events_derived_progress() FROM PUBLIC;

CREATE TRIGGER events_lineage BEFORE INSERT ON forge.events
    FOR EACH ROW WHEN (NEW.caused_by_seq IS NOT NULL)
    EXECUTE FUNCTION forge.events_lineage();
CREATE CONSTRAINT TRIGGER events_derived_progress AFTER INSERT ON forge.events
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW WHEN (NEW.caused_by_seq IS NOT NULL)
    EXECUTE FUNCTION forge.events_derived_progress();

DROP TRIGGER events_audit_present ON forge.events;
CREATE CONSTRAINT TRIGGER events_audit_present AFTER INSERT ON forge.events
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW WHEN (NEW.caused_by_seq IS NULL)
    EXECUTE FUNCTION forge.events_audit_present();

ALTER TABLE forge.events ENABLE ALWAYS TRIGGER events_lineage;
ALTER TABLE forge.events ENABLE ALWAYS TRIGGER events_derived_progress;
ALTER TABLE forge.events ENABLE ALWAYS TRIGGER events_audit_present;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('function', 'forge.events_lineage()'),
    ('function', 'forge.events_derived_progress()'),
    ('trigger', 'events_lineage on forge.events'),
    ('trigger', 'events_derived_progress on forge.events')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261027140000, "guarded_changes": ["forge.events", "forge.events_lineage", "forge.events_derived_progress", "forge.events.events_lineage", "forge.events.events_derived_progress", "forge.events.events_audit_present"]}');
