-- Step C6, review: a dead job no longer blocks its dedup_key. jobs_dedup
-- covered every state, so re-enqueueing the work of a dead keyed job (the
-- recovery of the job-dead-lettered runbook) was a silent no-op. Live jobs
-- (queued, running) and succeeded jobs still dedup; a dead one can be
-- enqueued again. enqueue's ON CONFLICT names the same predicate.
--
-- Also: jobs_ready_kind lets forge doctor (jobs.queue_depth) visit each kind
-- with ready jobs (a skip scan) and read its bounded count and its oldest
-- ready job without sampling across kinds.
-- forge.jobs is not guarded (tables.toml: guarded = false).

DROP INDEX forge.jobs_dedup;
CREATE UNIQUE INDEX jobs_dedup ON forge.jobs (kind, dedup_key)
    WHERE dedup_key IS NOT NULL AND state <> 'dead';
CREATE INDEX jobs_ready_kind ON forge.jobs (kind, run_after) WHERE state = 'queued';

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261027120000, "guarded_changes": []}');
