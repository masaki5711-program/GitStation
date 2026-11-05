-- feat/db-check-run-retention (ADR-0024 "実装で決めたこと（O3）" note):
-- the worker's retention of check runs walks the runs by age, oldest
-- first (forge-db checks::expire_old_runs). Without this index every batch
-- would scan the whole of forge.check_runs. No column, constraint or
-- trigger changes; the table stays audited on the project.

CREATE INDEX check_runs_updated_at ON forge.check_runs (updated_at, id);

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261119100000, "guarded_changes": []}');
