-- Plan 21 G1 (ADR-0029 amendment 2026-11-25): a job may ask that its
-- artifacts are kept past the 30-day retention (`artifacts: { keep: true }`
-- in `.forge-ci.yml`). The planner writes the request already decided:
-- true only when the job asked AND the pipeline is a push to a protected
-- branch (forge_ci::PlannedJob::keep_artifacts). The upload's record
-- decides again from the pipeline row (`source = 'push' AND protected`,
-- the same answer as StoredPipeline::secrets_allowed) and writes
-- `ci_artifacts.keep`; `ci_artifact_expiry` never takes a kept row.
--
-- DERIVED state (ci_jobs is kind = "derived"): written with the plan only;
-- no audit row. Rows of older pipelines read false (their artifacts expire
-- as before).
-- Locking: ADD COLUMN with a constant default is a catalog change on
-- PostgreSQL 16 (ACCESS EXCLUSIVE, brief; no rewrite).

ALTER TABLE forge.ci_jobs ADD COLUMN artifacts_keep boolean NOT NULL DEFAULT false;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20270110200000, "guarded_changes": []}');
