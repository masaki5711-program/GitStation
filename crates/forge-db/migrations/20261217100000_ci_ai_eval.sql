-- Plan 17 P17-2 (ADR-0034): the `forge/ai-eval` check — forge's worker
-- reads the AI evaluation report (`eval-report.json`) a pipeline's job left
-- as an artifact, compares it with the baseline in the repository and
-- writes the check of the pipeline's commit.
--
-- DERIVED state (ci_pipelines is kind = "derived" in tables.toml): set when
-- the pipeline is planned and by the worker's sweep; no audit row.
--   ai_eval_job     the planned job that declares the artifact path
--                   `eval-report.json` (forge_ci::EVAL_REPORT_PATH). NULL for
--                   every other pipeline: such a pipeline never gets a
--                   `forge/ai-eval` run (plan 17 §1.6, §9).
--   ai_eval_status  the pipeline status the check was last written for (the
--                   sweep's mark, like check_status for `ci/pipeline`); only
--                   with ai_eval_job.
-- Locking: ADD COLUMN without a default and ADD CONSTRAINT on new, NULL
-- columns are catalog changes (ACCESS EXCLUSIVE, brief); the index is built
-- over the pipelines that declare the report (none before this file).

ALTER TABLE forge.ci_pipelines
    ADD COLUMN ai_eval_job text CONSTRAINT ci_pipelines_ai_eval_job
        CHECK (ai_eval_job ~ '^[a-z0-9][a-z0-9._-]{0,62}$'),
    ADD COLUMN ai_eval_status text CONSTRAINT ci_pipelines_ai_eval_status CHECK (ai_eval_status IN
        ('pending', 'running', 'succeeded', 'failed', 'canceled', 'skipped', 'invalid', 'error')),
    ADD CONSTRAINT ci_pipelines_ai_eval
        CHECK (ai_eval_job IS NOT NULL OR ai_eval_status IS NULL);
-- The sweep's scan: declaring pipelines whose status is not yet the check's.
CREATE INDEX ci_pipelines_ai_eval_due ON forge.ci_pipelines (id)
    WHERE ai_eval_job IS NOT NULL AND ai_eval_status IS DISTINCT FROM status;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261217100000, "guarded_changes": []}');
