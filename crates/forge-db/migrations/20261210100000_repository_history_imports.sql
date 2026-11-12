-- 2026-11-12 (ADR-0008 amendment "history import", runbook push-checks):
-- the one-time history import allowance of an empty repository.
--
-- forge.repository_history_imports (audited on ('project', project_id)): at
-- most one row per repository, written by `forge admin repo allow-import`
-- (operator CLI only; `repository.history_import_allowed`) and used up by
-- the first push into the empty repository that the push checks accept
-- (`repository.history_imported`, with one
-- `repository.history_secret_imported` audit row per secret found only in
-- history). Audit only, no event.
--
--   allowed_at / expires_at  the allowance is open while consumed_at is NULL
--                            and now() < expires_at (at most 7 days: the
--                            CLI's bound, forge_core::history_import::MAX_TTL)
--   claimed_at               the push that holds it (review 2026-11-12, HIGH):
--   claimed_request_id       claimed atomically BEFORE its receive-pack runs
--                            (`UPDATE … WHERE claimed_at IS NULL AND
--                            consumed_at IS NULL AND expires_at > now()`), so
--                            of two concurrent first pushes only one gets the
--                            relaxed scan. Released when that push applied no
--                            ref; kept when its record failed after refs
--                            landed (never re-armed). Not audited by itself
--                            (unaudited columns): the claim is the push's,
--                            the audit row comes with the consumption.
--   consumed_at              when the first accepted push used it
--   consumed_request_id      that push's request id
--
-- project_id is the repository's own (composite foreign key to
-- repositories (id, project_id), the UNIQUE added here; review: an audit row
-- on the wrong project otherwise). A new allowance on the same repository
-- replaces the row (the CLI refuses a repository that has refs). The project
-- purge deletes the row.

ALTER TABLE forge.repositories
    ADD CONSTRAINT repositories_id_project UNIQUE (id, project_id);

CREATE TABLE forge.repository_history_imports (
    repository_id        bigint PRIMARY KEY,
    project_id           bigint NOT NULL REFERENCES forge.projects (id),
    allowed_at           timestamptz NOT NULL DEFAULT now(),
    expires_at           timestamptz NOT NULL,
    claimed_at           timestamptz,
    claimed_request_id   text,
    consumed_at          timestamptz,
    consumed_request_id  text,
    CONSTRAINT repository_history_imports_repository
        FOREIGN KEY (repository_id, project_id)
        REFERENCES forge.repositories (id, project_id),
    CONSTRAINT repository_history_imports_ttl CHECK (
        expires_at > allowed_at AND expires_at <= allowed_at + interval '7 days'),
    CONSTRAINT repository_history_imports_consumed CHECK (
        (consumed_at IS NULL) = (consumed_request_id IS NULL)),
    CONSTRAINT repository_history_imports_claimed CHECK (
        (claimed_at IS NULL) = (claimed_request_id IS NULL)),
    CONSTRAINT repository_history_imports_consumed_claimed CHECK (
        consumed_at IS NULL OR claimed_request_id = consumed_request_id),
    CONSTRAINT repository_history_imports_request_id CHECK (
        (consumed_request_id IS NULL OR length(consumed_request_id) BETWEEN 1 AND 128)
        AND (claimed_request_id IS NULL OR length(claimed_request_id) BETWEEN 1 AND 128))
);
CREATE INDEX repository_history_imports_project
    ON forge.repository_history_imports (project_id);

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF repository_id, project_id, allowed_at, expires_at,
        consumed_at, consumed_request_id
    ON forge.repository_history_imports
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id',
        'claimed_at,claimed_request_id', '');
ALTER TABLE forge.repository_history_imports ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.repository_history_imports TO forge_app;
GRANT SELECT ON forge.repository_history_imports TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.repository_history_imports')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261210100000, "guarded_changes": ["forge.repository_history_imports.audited_change_present"]}');
