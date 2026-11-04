-- M3 O3 re-review: rows that succeeded before last_success_at existed
-- (20261117100100) get their last successful run from last_finished_at, so
-- doctor's `reason=no_recent_success` does not flag them on the first run
-- after the upgrade. Operational state (tables.toml kind = "derived"): no
-- audit row. Idempotent: only rows still without the column.

UPDATE forge.repository_maintenance
    SET last_success_at = last_finished_at
    WHERE outcome = 'succeeded' AND last_success_at IS NULL AND last_finished_at IS NOT NULL;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261117100200, "guarded_changes": []}');
