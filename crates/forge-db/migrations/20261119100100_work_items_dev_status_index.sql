-- M3 I1d (ADR-0016 Amendment I1d): the paged development status
-- (`GET /api/v1/integrations/{id}/dev-status`) walks an integration's work
-- items in id order after a cursor. work_items_identity (integration_id,
-- external_project_id, ref) does not serve that order, so a backlog of
-- work items would be sorted on every page; this index makes each page one
-- ordered range scan.
--
-- Lock: a plain CREATE INDEX holds a SHARE lock on forge.work_items while it
-- builds, so work-item-sync's inserts (new references) wait for it. The
-- table is small at M3 (one row per kanflow reference ever named, a few
-- thousand), so the build is short; CONCURRENTLY is not used because the
-- migrator runs each file in a transaction.

CREATE INDEX work_items_integration_id ON forge.work_items (integration_id, id);

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261119100100, "guarded_changes": []}');
