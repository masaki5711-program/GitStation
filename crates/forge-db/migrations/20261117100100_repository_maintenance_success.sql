-- M3 O3 independent review (ADR-0024 "O3 の独立レビュー"): the maintenance
-- ledger also keeps the last successful run.
--
--   last_success_at  when a run last ended `succeeded`. doctor
--                    `repositories.maintenance` reports a ready repository
--                    whose last success (or, never maintained, its
--                    `ready_at`) is older than twice the interval as
--                    FG-GIT-0018 reason=no_recent_success — a repository
--                    skipped every night (a push always in progress, a gc
--                    always running) is never `failed` nor overdue
--                    (`next_due_at` moves an hour ahead with every skip).
--
-- The index repository_maintenance_due (20261117100000) is dropped: the
-- scheduler orders by coalesce(next_due_at, repositories.ready_at) over a
-- LEFT JOIN, which no index on next_due_at alone serves.

ALTER TABLE forge.repository_maintenance ADD COLUMN last_success_at timestamptz;

DROP INDEX forge.repository_maintenance_due;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261117100100, "guarded_changes": []}');
