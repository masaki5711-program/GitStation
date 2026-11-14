-- Plan 17 P17-2 review (ADR-0034 §4): the `forge/ai-eval` sweep's read
-- failures are counted and backed off, so a pipeline whose report or
-- baseline cannot be read ends as an `error` check after a bounded number
-- of passes (never queued forever) and does not starve the others.
--
-- DERIVED state (ci_pipelines is kind = "derived"): written by the worker's
-- sweep only; no audit row.
--   ai_eval_failures  read failures since the check was last written (reset
--                     by every write)
--   ai_eval_retry_at  not before this time (exponential backoff, at most 10
--                     minutes); NULL: due now
-- Locking: ADD COLUMN with a constant default is a catalog change on
-- PostgreSQL 16 (ACCESS EXCLUSIVE, brief); the index is rebuilt over the
-- declaring pipelines only.

ALTER TABLE forge.ci_pipelines
    ADD COLUMN ai_eval_failures integer NOT NULL DEFAULT 0
        CONSTRAINT ci_pipelines_ai_eval_failures CHECK (ai_eval_failures BETWEEN 0 AND 1000),
    ADD COLUMN ai_eval_retry_at timestamptz;
DROP INDEX forge.ci_pipelines_ai_eval_due;
-- The sweep's scan: declaring pipelines whose status is not yet the
-- check's, by when they may be tried again.
CREATE INDEX ci_pipelines_ai_eval_due ON forge.ci_pipelines (coalesce(ai_eval_retry_at, created_at), id)
    WHERE ai_eval_job IS NOT NULL AND ai_eval_status IS DISTINCT FROM status;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261217100100, "guarded_changes": []}');
