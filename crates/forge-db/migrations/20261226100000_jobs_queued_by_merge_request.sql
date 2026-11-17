-- Merge load test 2026-11-17 (docs/handoff/2026-11-17-merge-load-test.md,
-- F2): at most one QUEUED job per (kind, merge request) for the
-- per-request jobs (merge_request.mergeability, merge_request.conflict_scan).
-- A new request deletes the queued jobs of older requests of the same merge
-- request in the statement that enqueues its own (forge-db
-- jobs::enqueue_superseding_on); before, every push to a target left one
-- queued job per open merge request and per push (1091 queued at 200 open
-- merge requests), each of which only found its seq superseded.
--
-- The index serves that DELETE (kind, the payload's merge_request_id) over
-- the queued rows only, which stay few. No column, constraint or trigger
-- changes; forge.jobs is not guarded (tables.toml: guarded = false).
--
-- Locking: a plain CREATE INDEX (SHARE lock on forge.jobs for the build:
-- enqueues and claims wait, reads continue). CONCURRENTLY is not possible:
-- sqlx runs each migration in a transaction.

CREATE INDEX jobs_queued_merge_request ON forge.jobs (kind, (payload ->> 'merge_request_id'))
    WHERE state = 'queued';

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261226100000, "guarded_changes": []}');
