-- Step C3 (ADR-0016 §1, §6): an event commits only together with an audit
-- row of the same transaction. ChangeTx writes both; a write that bypasses
-- it fails at COMMIT with XF002 (FG-DB-0013). Derived events (caused_by_seq
-- set, written by subscribers, C5) are checked by their own trigger there.

CREATE FUNCTION forge.events_audit_present() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM ONLY forge.audit_log AS a WHERE a.txid = pg_catalog.pg_current_xact_id()
    ) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF002',
            MESSAGE = 'an event of forge.events has no audit row in its transaction';
    END IF;
    RETURN NULL;
END
$$;
REVOKE ALL ON FUNCTION forge.events_audit_present() FROM PUBLIC;

CREATE CONSTRAINT TRIGGER events_audit_present AFTER INSERT ON forge.events
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW WHEN (NEW.caused_by_seq IS NULL)
    EXECUTE FUNCTION forge.events_audit_present();
ALTER TABLE forge.events ENABLE ALWAYS TRIGGER events_audit_present;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('function', 'forge.events_audit_present()'),
    ('trigger', 'events_audit_present on forge.events')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261027100400, "guarded_changes": ["forge.events", "forge.events_audit_present", "forge.events.events_audit_present"]}');
