-- M3 I1d (ADR-0016 Amendment I1d): the paged development status
-- (`GET /api/v1/integrations/{id}/dev-status`) walks an integration's work
-- items in id order after a cursor. work_items_identity (integration_id,
-- external_project_id, ref) does not serve that order, so a backlog of
-- work items would be sorted on every page; this index makes each page one
-- ordered range scan.

CREATE INDEX work_items_integration_id ON forge.work_items (integration_id, id);

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261119100100, "guarded_changes": []}');
