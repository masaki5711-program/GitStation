-- Plan 15 MR 5: projects.created_by — the user a project was created by
-- (`projects::create` writes `NewProject::creator`, the user who becomes its
-- first direct Owner; the same user the `project.created` audit row names
-- as `details.owner_id`). NULL means "creator unknown": a project created
-- before this column whose creation the audit log does not name. Users are
-- never deleted (as for deleted_by), so a plain reference, no ON DELETE.
-- `GET /api/v1/projects?created_by=me` filters on it after `authorize()`.
--
-- Backfill: from the earliest (lowest id) succeeded `project.created`
-- audit row of each project (resource_type 'project', resource_id = the
-- project), so the answer is deterministic even if a project had more
-- than one such row. The audit log is read only. The UPDATE runs while the audit trigger still lists the old
-- columns only (created_by is not in its UPDATE OF list yet), so it needs
-- no per-project audit row: it copies what the audit log already records
-- and adds no new fact; this file's own `db.migrate` row is its trace.
-- Rows without such an audit row, or whose named user no longer resolves,
-- stay NULL. Idempotent: only rows still NULL are touched.
--
-- Then the audit trigger is re-created with created_by in UPDATE OF, so
-- any later change of it needs a `project` audit row like every other
-- projects column.
--
-- Index: projects_created_by (created_by, id), partial on NOT NULL — the
-- `?created_by=me` page scan (`lists::PROJECT_CREATED_PAGE_SQL`, `WHERE
-- created_by = $5 AND id > $1 ORDER BY id LIMIT $4`) walks it in id order.
--
-- Lock impact: sqlx runs this file in one transaction, so the ALTER, the
-- backfill, the CREATE INDEX and the trigger swap all hold ACCESS
-- EXCLUSIVE on forge.projects until COMMIT (reads and writes of projects
-- wait). forge.audit_log has no (resource_type, resource_id) index, so the
-- backfill scans it once. Accepted: projects is small, this runs once at
-- deploy, and CREATE INDEX CONCURRENTLY is impossible inside the sqlx
-- transaction.

ALTER TABLE forge.projects ADD COLUMN created_by bigint REFERENCES forge.users (id);

UPDATE forge.projects AS p
    SET created_by = u.id
    FROM (SELECT DISTINCT ON (a.resource_id) a.resource_id, a.details ->> 'owner_id' AS owner_id
            FROM forge.audit_log AS a
            WHERE a.action = 'project.created' AND a.outcome = 'succeeded'
              AND a.resource_type = 'project'
            ORDER BY a.resource_id, a.id) AS first
    JOIN forge.users AS u ON first.owner_id ~ '^[0-9]{1,19}$' AND u.id::text = first.owner_id
    WHERE p.created_by IS NULL AND first.resource_id = p.id;

CREATE INDEX projects_created_by ON forge.projects (created_by, id) WHERE created_by IS NOT NULL;

DROP TRIGGER audited_change_present ON forge.projects;
CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, display_name, description, visibility, created_at,
        deleted_at, purge_after, deleted_by, created_by ON forge.projects
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'id', 'updated_at', '');
ALTER TABLE forge.projects ENABLE ALWAYS TRIGGER audited_change_present;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261209143000, "guarded_changes": ["forge.projects.audited_change_present"]}');
