-- Plan 13 Q13 (ADR-0026 "決まったこと", 2026-11-10): the document lint's
-- migration setting. Broken document references (`ref_broken`) are errors
-- in `forge/doc-lint`; a repository may lower them to warnings while it
-- migrates its documents, until a date.
--
-- forge.doc_lint_migrations (audited on ('project', project_id)): at most one
-- row per project (its one repository), written by
-- `PUT /api/v1/projects/{id}/doc-lint/migration` (DocLintMigrationManage:
-- people only, Maintainer; `doc_lint_migration.set`) and removed by `DELETE`
-- (`doc_lint_migration.cleared`), audit only, no event.
--
--   warn_until  the last day (UTC) on which broken references are warnings;
--               from the next day they are errors again by themselves (the
--               check compares it with the database's date, no write).
--   set_by      the person who set the date last; set_at when.
--
-- The CHECK is the backstop of the route's rule (forge-db compares with the
-- database's date in the same transaction): the date is after the day it
-- was set and at most 180 days after it (forge_core::doc_lint_migration::
-- MAX_DAYS). An expired row stays until it is set again or cleared (the
-- screen shows it as ended); the project purge deletes it.

CREATE TABLE forge.doc_lint_migrations (
    project_id  bigint PRIMARY KEY REFERENCES forge.projects (id),
    warn_until  date NOT NULL,
    set_by      bigint NOT NULL REFERENCES forge.users (id),
    set_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT doc_lint_migrations_until CHECK (
        warn_until > (set_at AT TIME ZONE 'UTC')::date
        AND warn_until <= (set_at AT TIME ZONE 'UTC')::date + 180)
);
CREATE INDEX doc_lint_migrations_set_by ON forge.doc_lint_migrations (set_by);

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF project_id, warn_until, set_by, set_at
    ON forge.doc_lint_migrations
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.doc_lint_migrations ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.doc_lint_migrations TO forge_app;
GRANT SELECT ON forge.doc_lint_migrations TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.doc_lint_migrations')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261203100000, "guarded_changes": ["forge.doc_lint_migrations.audited_change_present"]}');
