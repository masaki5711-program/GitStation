-- Plan 20 K2 (ADR-0040): the search index. DERIVED state (tables.toml kind =
-- "derived"): rebuilt from the source tables at any time (`forge search
-- reindex`), written by the `search-index` subscriber and the rebuild, never
-- a decision of a person, so a change needs no audit row.
--
-- search_entries: one row per searchable thing.
--   source_kind        project | merge_request (K4 adds document, I-1 issue)
--   project_id         the project it belongs to. No foreign key on purpose:
--                      the project purge stays unchanged; a row of a gone
--                      project is dropped by the per-hit authorize() (its
--                      project has no record) and deleted by the subscriber
--                      (`project.purged`) and the rebuild.
--   source_key         the thing within the kind and project: '' for the
--                      project itself, the merge request's iid (decimal)
--   title / body_text  what a hit shows (the snippet is cut from body_text,
--                      at most 256 KB; past it `truncated`)
--   tokens             the forge-core::search_text vector (title A,
--                      paths / ids B, body C), cast from its text form
--   classification     public | internal | confidential: an AI audience
--                      never sees a confidential row (projects and merge
--                      requests are `internal`)
--   source_updated_at  the source's own change time (ranking: newer first
--                      among equals)
--
-- search_index_state: per project, the last indexing outcome (K4 fills the
-- indexed SHA of documents): `state` ok | failed, the last FG code, attempts.

CREATE TABLE forge.search_entries (
    id                 bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    source_kind        text NOT NULL CONSTRAINT search_entries_source_kind
        CHECK (source_kind IN ('project', 'merge_request', 'document', 'issue')),
    project_id         bigint NOT NULL CONSTRAINT search_entries_project_positive
        CHECK (project_id > 0),
    source_key         text NOT NULL CONSTRAINT search_entries_source_key
        CHECK (octet_length(source_key) <= 1024),
    title              text NOT NULL CONSTRAINT search_entries_title
        CHECK (octet_length(title) <= 4096),
    body_text          text NOT NULL DEFAULT '' CONSTRAINT search_entries_body
        CHECK (octet_length(body_text) <= 262144),
    tokens             tsvector NOT NULL,
    classification     text NOT NULL DEFAULT 'internal'
        CONSTRAINT search_entries_classification
        CHECK (classification IN ('public', 'internal', 'confidential')),
    truncated          boolean NOT NULL DEFAULT false,
    source_updated_at  timestamptz NOT NULL,
    indexed_at         timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT search_entries_source UNIQUE (source_kind, project_id, source_key)
);
-- The search itself (`tokens @@ $1::tsquery`).
CREATE INDEX search_entries_tokens ON forge.search_entries USING gin (tokens);
-- A project's rows (delete on purge, rebuild of one project).
CREATE INDEX search_entries_project ON forge.search_entries (project_id);

CREATE TABLE forge.search_index_state (
    project_id         bigint PRIMARY KEY CONSTRAINT search_index_state_project_positive
        CHECK (project_id > 0),
    indexed_sha        text CONSTRAINT search_index_state_sha
        CHECK (indexed_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    state              text NOT NULL CONSTRAINT search_index_state_state
        CHECK (state IN ('ok', 'failed')),
    last_error_code    text CONSTRAINT search_index_state_error_code
        CHECK (last_error_code ~ '^FG-[A-Z]+-[0-9]{4}$'),
    attempts           integer NOT NULL DEFAULT 0 CONSTRAINT search_index_state_attempts
        CHECK (attempts >= 0),
    updated_at         timestamptz NOT NULL DEFAULT now()
);

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.search_entries TO forge_app;
GRANT SELECT ON forge.search_entries TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.search_index_state TO forge_app;
GRANT SELECT ON forge.search_index_state TO forge_ro;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20270107100000, "guarded_changes": []}');
