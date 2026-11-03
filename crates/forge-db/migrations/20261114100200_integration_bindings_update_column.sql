-- M3 I1a re-review (database): 20261114100100 granted forge_app UPDATE on
-- the whole forge.integration_bindings table, but the commands only clear
-- granted_membership (integrations::clear_grant, on a removal or a role
-- change of the granted membership). A binding's integration, project,
-- external project, creator and creation time never change after the
-- INSERT: the grant is narrowed to the one column (a moved external
-- project is an unbind and a new bind).

REVOKE UPDATE ON forge.integration_bindings FROM forge_app;
GRANT UPDATE (granted_membership) ON forge.integration_bindings TO forge_app;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261114100200, "guarded_changes": ["forge.integration_bindings"]}');
