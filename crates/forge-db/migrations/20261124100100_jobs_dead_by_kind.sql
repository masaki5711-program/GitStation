-- M3 X2 review (rust HIGH): the worker's spec drift sweep answers
-- `check.spec_drift` jobs that died without a run (forge-db
-- checks::spec_drift::unanswered_dead_jobs: dead jobs of one kind, by
-- finish time). Dead jobs are few; without this index every sweep (each
-- minute) would scan forge.jobs. No column, constraint or trigger changes.
--
-- Locking: a plain CREATE INDEX (SHARE lock on forge.jobs for the build:
-- enqueues and claims wait, reads continue). CONCURRENTLY is not possible:
-- sqlx runs each migration in a transaction.

CREATE INDEX jobs_dead_kind ON forge.jobs (kind, finished_at) WHERE state = 'dead';

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261124100100, "guarded_changes": []}');
