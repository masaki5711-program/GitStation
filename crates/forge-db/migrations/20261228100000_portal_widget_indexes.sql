-- ADR-0036 (review of the portal widgets, 2026-11-17): the widgets are polled
-- per person, so the two reads that had no index of their own get one.
--
--   audit_log_spec_releases  `my-reviews?kind=spec` asks, per candidate
--       project, whether it ever released a specification (an EXISTS on its
--       `spec.released` audit rows; review::user_lists::widget_sql). Without
--       this the question walks the project's whole audit history through
--       audit_log_project_time on every poll.
--   ci_pipelines_ref_newest  the `pipelines` widget reads the newest pipeline
--       of one ref per project (portal::latest_pipelines); ci_pipelines_project
--       (project_id, id DESC) walks the project's pipelines until one of the
--       ref is found.
--
-- Not CONCURRENTLY, as 20261111100100: DDL on forge.audit_log (a guarded
-- table) is expected only in the transaction of its db.migrate row. The
-- `spec.released` rows are a very small part of the log; the build holds a
-- SHARE lock on each table (writes wait) for its duration — on a large
-- installation run the migration in a quiet window (runbook db-migration).

CREATE INDEX audit_log_spec_releases
    ON forge.audit_log (project_id) WHERE action = 'spec.released';

CREATE INDEX ci_pipelines_ref_newest
    ON forge.ci_pipelines (project_id, ref, id DESC);

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261228100000, "guarded_changes": ["forge.audit_log"]}');
