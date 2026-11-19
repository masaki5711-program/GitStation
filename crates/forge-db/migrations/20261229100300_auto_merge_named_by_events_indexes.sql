-- Plan 18 S3, review (ADR-0037 (a), the S3 review addendum): the auto-merge
-- worker reads up to 10 x 200 sequenced events per wake
-- (forge-db review::auto_merge::named_by_events) and, for each
-- `check.completed`, finds the open merge request whose head it is and the
-- merge request whose merge result it is. Neither lookup had an index:
-- merge_requests_open_project is (project_id, id) and
-- merge_result_refs_project is (project_id), so the read walked every open
-- merge request of the project and every merge result ref.
--
-- Measured before this migration (a fresh database: 5 000 open and 15 000
-- closed merge requests, 60 000 merge result refs and 3 000 events in one
-- project; one 200-event read): a merge join filtering 670 000 rows plus a
-- sequential scan of the refs, 55.6 ms execution (40.8 ms mean over 20
-- reads). With these two indexes: two index lookups per event, 1.1 ms
-- (4.3 ms mean, client round trip included).
--
-- - merge_requests_open_head (project_id, head_sha) WHERE state = 'open':
--   the open merge request a check run's sha is the head of. Partial like
--   the other open-row indexes (20261208163000): closed and merged rows stay
--   out.
-- - merge_result_refs_result (project_id, result_sha): the merge request a
--   check run on a merge result names (`ci/pipeline` reports there).
--
-- No column, constraint or trigger changes; nothing guarded changes (the
-- db.migrate row lists none, as 20261208163000).
--
-- Locking: a plain CREATE INDEX takes a SHARE lock on each table for its
-- build: writes (opening, editing, pushing, closing a merge request; a new
-- merge result) wait until it finishes, reads continue. Both tables hold a
-- row per merge request (merge results: a few per merge request), thousands
-- to tens of thousands of rows, so the build is short (not timed on its own:
-- the whole measurement, seeding included, took 8 s). CREATE INDEX CONCURRENTLY is not possible here:
-- sqlx runs each migration in a transaction. On a large installation run it
-- in a quiet window (runbook db-migration).

CREATE INDEX merge_requests_open_head ON forge.merge_requests (project_id, head_sha)
    WHERE state = 'open';
CREATE INDEX merge_result_refs_result ON forge.merge_result_refs (project_id, result_sha);

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261229100300, "guarded_changes": []}');
