-- Step D1 (ADR-0015, ADR-0016 §1): users, their namespaces and the
-- tombstones of released paths. All three are state tables
-- (tables.toml kind = "audited"): every change commits only with a
-- succeeded audit row for its resource (forge.audited_change_present,
-- migration 20261028100000).
--
-- users: one row per account. A human has an email (unique ignoring case);
-- service accounts and bots may not. employee_number is unique when present,
-- external_id per auth_source. username is a top-level segment (the same
-- CHECK as namespaces.segment; the reserved words are refused by forge-core).
-- The read pool (forge_ro) sees every column but email, employee_number and
-- external_id. The field limits are mirrored by forge-db users::create
-- (FG-AUTH-0006), so the CHECKs are a backstop.
--
-- namespaces: the path tree of users, groups and projects (ADR-0015 schema B
-- plus kind / owner_id, so the owner's audit row covers its namespace).
-- segment follows PathSegment::parse (forge-core); full_path is compared
-- byte-wise (COLLATE "C"). A subtree rename rewrites full_path / depth of
-- every descendant under one audit row (txn-audited columns).
--
-- owner_id is polymorphic (a user, group or project id by kind), so it has
-- no foreign key (ADR-0015 Amendment D1).
--
-- namespace_tombstones: a released path stays unusable (ADR-0015): `moved`
-- redirects to namespace_id, `deleted` does not. The application inserts and
-- deletes them, never updates. namespace_id / parent_namespace_id become NULL
-- only through ON DELETE SET NULL when a namespace is purged (a `moved`
-- tombstone without a target is then treated as `deleted`); that is why they
-- are unaudited columns. Every writer of a path (create, and D5 rename / move
-- / delete / release) first takes the transaction advisory lock of the path
-- (forge-db namespaces::lock_path), so a create never misses a tombstone
-- written concurrently.
--
-- No role gets TRUNCATE (it would bypass the row triggers); doctor compares
-- the exact ACLs (bootstrap/schema.rs).

CREATE TABLE forge.users (
    id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    kind             text NOT NULL
        CONSTRAINT users_kind CHECK (kind IN ('human', 'service_account', 'bot')),
    username         text COLLATE "C" NOT NULL CONSTRAINT users_username_key UNIQUE
        CONSTRAINT users_username CHECK (
            username ~ '^[a-z0-9]([a-z0-9._-]{0,62}[a-z0-9])?$'
            AND strpos(username, '..') = 0
            AND username !~ '\.(git|atom|wiki)$'
            AND split_part(username, '.', 1) !~ '^(con|prn|aux|nul|com[1-9]|lpt[1-9])$'),
    display_name     text NOT NULL CONSTRAINT users_display_name_length
        CHECK (length(display_name) BETWEEN 1 AND 255),
    email            text CONSTRAINT users_email_length
        CHECK (length(email) BETWEEN 1 AND 320),
    employee_number  text CONSTRAINT users_employee_number_length
        CHECK (length(employee_number) BETWEEN 1 AND 64),
    external_id      text CONSTRAINT users_external_id_length
        CHECK (length(external_id) BETWEEN 1 AND 255),
    auth_source      text NOT NULL DEFAULT 'local'
        CONSTRAINT users_auth_source CHECK (auth_source IN ('local', 'ldap')),
    state            text NOT NULL DEFAULT 'active'
        CONSTRAINT users_state CHECK (state IN ('active', 'disabled')),
    is_admin         boolean NOT NULL DEFAULT false,
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT users_human_email CHECK (kind <> 'human' OR email IS NOT NULL)
);
CREATE UNIQUE INDEX users_email_lower ON forge.users (lower(email)) WHERE email IS NOT NULL;
CREATE UNIQUE INDEX users_employee_number ON forge.users (employee_number)
    WHERE employee_number IS NOT NULL;
CREATE UNIQUE INDEX users_external_id ON forge.users (auth_source, external_id)
    WHERE external_id IS NOT NULL;

CREATE TABLE forge.namespaces (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    parent_id   bigint REFERENCES forge.namespaces (id),
    kind        text NOT NULL
        CONSTRAINT namespaces_kind CHECK (kind IN ('user', 'group', 'project')),
    owner_id    bigint NOT NULL,
    segment     text COLLATE "C" NOT NULL CONSTRAINT namespaces_segment CHECK (
        segment ~ '^[a-z0-9]([a-z0-9._-]{0,62}[a-z0-9])?$'
        AND strpos(segment, '..') = 0
        AND segment !~ '\.(git|atom|wiki)$'
        AND split_part(segment, '.', 1) !~ '^(con|prn|aux|nul|com[1-9]|lpt[1-9])$'),
    full_path   text COLLATE "C" NOT NULL CONSTRAINT namespaces_full_path_key UNIQUE
        CONSTRAINT namespaces_full_path_length CHECK (length(full_path) <= 255),
    depth       smallint NOT NULL CONSTRAINT namespaces_depth CHECK (depth BETWEEN 1 AND 20),
    created_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT namespaces_sibling_key UNIQUE NULLS NOT DISTINCT (parent_id, segment),
    CONSTRAINT namespaces_top_level CHECK ((parent_id IS NULL) = (depth = 1)),
    CONSTRAINT namespaces_top_level_path CHECK (parent_id IS NOT NULL OR full_path = segment)
);
-- Every owner has exactly one namespace (owner_id has no FK, so this is the
-- database's only guard of the 1:1).
CREATE UNIQUE INDEX namespaces_owner ON forge.namespaces (kind, owner_id);

CREATE TABLE forge.namespace_tombstones (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    old_path      text COLLATE "C" NOT NULL CONSTRAINT namespace_tombstones_old_path_key UNIQUE
        CONSTRAINT namespace_tombstones_old_path_length CHECK (length(old_path) <= 255),
    parent_namespace_id bigint REFERENCES forge.namespaces (id) ON DELETE SET NULL,
    namespace_id  bigint REFERENCES forge.namespaces (id) ON DELETE SET NULL,
    kind          text NOT NULL
        CONSTRAINT namespace_tombstones_kind CHECK (kind IN ('moved', 'deleted')),
    owner_at_write bigint REFERENCES forge.users (id),
    cause_type    text NOT NULL CONSTRAINT namespace_tombstones_cause_type
        CHECK (cause_type IN ('user', 'group', 'project')),
    cause_id      bigint NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, kind, username, display_name, email, employee_number,
        external_id, auth_source, state, is_admin, created_at ON forge.users
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('user', 'id', 'updated_at', '');
ALTER TABLE forge.users ENABLE ALWAYS TRIGGER audited_change_present;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, parent_id, kind, owner_id, segment, full_path, depth,
        created_at ON forge.namespaces
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('=kind', 'owner_id', '', 'full_path,depth');
ALTER TABLE forge.namespaces ENABLE ALWAYS TRIGGER audited_change_present;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, old_path, kind, owner_at_write, cause_type, cause_id,
        created_at ON forge.namespace_tombstones
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('=cause_type', 'cause_id',
        'namespace_id,parent_namespace_id', '');
ALTER TABLE forge.namespace_tombstones ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT, INSERT, UPDATE ON forge.users TO forge_app;
GRANT SELECT (id, kind, username, display_name, auth_source, state, is_admin, created_at,
    updated_at) ON forge.users TO forge_ro;
GRANT SELECT, INSERT, UPDATE ON forge.namespaces TO forge_app;
GRANT SELECT ON forge.namespaces TO forge_ro;
GRANT SELECT, INSERT, DELETE ON forge.namespace_tombstones TO forge_app;
GRANT SELECT ON forge.namespace_tombstones TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.users'),
    ('trigger', 'audited_change_present on forge.namespaces'),
    ('trigger', 'audited_change_present on forge.namespace_tombstones')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261028100100, "guarded_changes": ["forge.users.audited_change_present", "forge.namespaces.audited_change_present", "forge.namespace_tombstones.audited_change_present"]}');
