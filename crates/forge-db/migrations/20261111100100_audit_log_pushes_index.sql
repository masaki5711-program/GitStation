-- M3 R5 review round 1 (ADR-0024 §8.3, "実装で決めたこと（R5）"): the committer
-- rule reads a project's `repository.pushed` audit rows (review::push_coverage)
-- at every evaluation of approvals. The audit log grows without bound, so the
-- read gets a partial index of exactly those rows, in id order per project.
--
-- Not CONCURRENTLY: DDL on forge.audit_log (a guarded table) is expected only
-- in the transaction of its db.migrate row (bootstrap/records.rs, ADR-0018 §3),
-- and a `-- no-transaction` migration is one statement with no such row. The
-- plain build holds a SHARE lock on the audit log (writes wait) for the build;
-- the rows of repository.pushed are a small part of the log. On a large
-- installation run the migration in a quiet window (runbook db-migration).

CREATE INDEX audit_log_pushes
    ON forge.audit_log (project_id, id) WHERE action = 'repository.pushed';

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261111100100, "guarded_changes": ["forge.audit_log"]}');
