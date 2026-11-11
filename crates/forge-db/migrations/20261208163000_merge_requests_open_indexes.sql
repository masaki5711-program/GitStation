-- feat/web-user-merge-requests (plan 15 MR 6): `GET /api/v1/user/merge-requests
-- ?scope=` and `…/counts` read one person's open merge requests across
-- projects, newest first by id (forge-db review::user_lists, keyset
-- `id < before`, `ORDER BY id DESC LIMIT n`). Two partial indexes on the open
-- rows (the lists never read another state; closed and merged rows, the
-- bulk of the table over time, stay out of them):
--
-- - merge_requests_open_author (author_id, id): `authored` and `attention`
--   (`author_id = $1 AND state = 'open'`), read in index order.
-- - merge_requests_open_project (project_id, id): `approvable`, one bounded
--   newest-first probe per project the person is a member of (a LATERAL
--   `ORDER BY id DESC LIMIT n` each), then the newest n of those; without
--   it each probe would sort the project's open rows
--   (merge_requests_project_state orders by iid, not id).
--
-- No column, constraint or trigger changes; the table stays audited on the
-- project.
--
-- Locking: a plain CREATE INDEX takes a SHARE lock on forge.merge_requests
-- for the build: writes (opening, editing, closing a merge request, the
-- version job) wait until it finishes, reads continue. The table holds one
-- row per merge request ever opened (thousands at M4), so the wait is short.
-- CREATE INDEX CONCURRENTLY is not possible here: sqlx runs each migration
-- in a transaction.

CREATE INDEX merge_requests_open_author ON forge.merge_requests (author_id, id)
    WHERE state = 'open';
CREATE INDEX merge_requests_open_project ON forge.merge_requests (project_id, id)
    WHERE state = 'open';

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261208163000, "guarded_changes": []}');
