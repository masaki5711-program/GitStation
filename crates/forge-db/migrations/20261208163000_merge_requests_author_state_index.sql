-- feat/web-user-merge-requests (plan 15 MR 6): `GET /api/v1/user/merge-requests
-- ?scope=authored|attention` and `…/counts` read one person's open merge
-- requests across every project, newest first (forge-db
-- review::user_merge_requests: `author_id = $1 AND state = 'open'`, keyset
-- `id < before`, `ORDER BY id DESC`). Without this index each page would
-- scan the whole of forge.merge_requests (the only index on the author was
-- none: merge_requests_project_state starts at the project). The id is the
-- third column so the keyset page is read in index order. No column,
-- constraint or trigger changes; the table stays audited on the project.
--
-- Locking: a plain CREATE INDEX takes a SHARE lock on forge.merge_requests
-- for the build: writes (opening, editing, closing a merge request, the
-- version job) wait until it finishes, reads continue. The table holds one
-- row per merge request ever opened (thousands at M4), so the wait is short.
-- CREATE INDEX CONCURRENTLY is not possible here: sqlx runs each migration
-- in a transaction.

CREATE INDEX merge_requests_author_state ON forge.merge_requests (author_id, state, id);

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261208163000, "guarded_changes": []}');
