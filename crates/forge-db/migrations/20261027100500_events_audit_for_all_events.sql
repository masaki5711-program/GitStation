-- Step C3, review: every event needs an audit row in its transaction,
-- derived events (caused_by_seq set) included. The exemption for derived
-- events relied on the subscriber checks of step C5, which do not exist
-- yet; C5 replaces this trigger together with its own derived-event check.

DROP TRIGGER events_audit_present ON forge.events;
CREATE CONSTRAINT TRIGGER events_audit_present AFTER INSERT ON forge.events
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    EXECUTE FUNCTION forge.events_audit_present();
ALTER TABLE forge.events ENABLE ALWAYS TRIGGER events_audit_present;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261027100500, "guarded_changes": ["forge.events", "forge.events.events_audit_present"]}');
