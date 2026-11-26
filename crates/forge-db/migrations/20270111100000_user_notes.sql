-- Plan 20 N1 (ADR-0040 §6, option N-B): a person's notes are an ordinary
-- private project `<user>/notes` in their own namespace; one row here
-- points at it (`POST /api/v1/user/notes` writes it last, after the project,
-- its repository and its first commit exist).
--
-- Derived kind in the ledger like forge.project_wikis: the project's own
-- creation is audited (`project.created`); this row is only the pointer.
-- The purge of the project removes the row under its `project.purged` row.
-- A pointer whose project was moved out of `<user>/notes` or is pending
-- deletion is stale (FG-KNOW-0005) and is replaced by the next creation.
--
--   user_id     the person (at most one notes project).
--   project_id  the notes project (the notes of at most one person).
CREATE TABLE forge.user_notes (
    user_id     bigint PRIMARY KEY REFERENCES forge.users (id),
    project_id  bigint NOT NULL REFERENCES forge.projects (id)
        CONSTRAINT user_notes_project_once UNIQUE,
    created_at  timestamptz NOT NULL DEFAULT now()
);

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.user_notes TO forge_app;
GRANT SELECT ON forge.user_notes TO forge_ro;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20270111100000, "guarded_changes": []}');
