-- M3 C1 review (security MEDIUM): a check namespace is granted to a
-- service account for ONE project (the approved design: account × project
-- × namespace). A grant for every project let any project's Maintainer add
-- another team's ci-granted service account as a Developer and report a
-- required check — lifting a merge condition they could not remove.
--
-- check_namespace_grants gains project_id (FK, removed by the project
-- purge); a grant is unique per (user_id, project_id, namespace). The table
-- is empty here: 20261115100000 and this migration ship together in C1, so
-- the NOT NULL column needs no backfill — a row would make this migration
-- fail rather than guess a project. The grants are now audited on the
-- project (resource `project`, id `project_id`, like the check runs), so the
-- purge's `project.purged` row covers their deletion; the audit trigger is
-- re-created with the new column.

ALTER TABLE forge.check_namespace_grants
    ADD COLUMN project_id bigint NOT NULL REFERENCES forge.projects (id);
ALTER TABLE forge.check_namespace_grants DROP CONSTRAINT check_namespace_grants_pkey;
ALTER TABLE forge.check_namespace_grants
    ADD CONSTRAINT check_namespace_grants_pkey PRIMARY KEY (user_id, project_id, namespace);
DROP INDEX forge.check_namespace_grants_namespace;
-- The reporters of a project's namespace (the report's grant read is the
-- primary key; doctor `checks.reporters`, `unreportable_checks`, the purge).
CREATE INDEX check_namespace_grants_project
    ON forge.check_namespace_grants (project_id, namespace);

DROP TRIGGER audited_change_present ON forge.check_namespace_grants;
CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF user_id, project_id, namespace, granted_by, created_at
    ON forge.check_namespace_grants
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.check_namespace_grants ENABLE ALWAYS TRIGGER audited_change_present;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.check_namespace_grants')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261115100100, "guarded_changes": ["forge.check_namespace_grants.audited_change_present"]}');
