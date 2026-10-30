-- Step D4 MR1 (ADR-0004, ADR-0008, ADR-0015, ADR-0016 §1, ADR-0018):
-- projects, their repository, their direct members and their protected ref
-- rules. All four are state tables (tables.toml kind = "audited") audited on
-- the project: every change commits only with a succeeded audit row for
-- resource ('project', projects.id) in its transaction
-- (forge.audited_change_present, migration 20261028100000).
--
-- projects: one row per project. Its path lives in forge.namespaces
-- (kind = 'project', owner_id = projects.id), like users and groups: the
-- unique index namespaces_owner (kind, owner_id) keeps the 1:1, and one
-- `project` audit row covers both (ADR-0015 Amendment D4: no namespace_id
-- column here). deleted_at / purge_after are set together or not at all
-- (D5 writes them; purge_after comes after deleted_at). No DELETE grant yet
-- (D5 adds purge).
--
-- repositories: one per project in M1 (project_id UNIQUE). kind
-- (spec | system, ADR-0004) is fixed at creation (step D plan Q3): forge_app
-- may UPDATE only state and ready_at; ready_at is set exactly when the state
-- is 'ready', and the state moves only provisioning -> ready (guard trigger
-- repositories_state_forward). provision_token marks the bare repository the provisioning job
-- creates (written into its config before the rename, so a retry adopts only
-- its own directory, D4 option A); the read pool never sees it (ADR-0018).
-- No DELETE grant yet (D5).
--
-- project_members: direct memberships. Inherited roles (ancestor groups, the
-- owner of a personal namespace) are computed, never stored. Bots are never
-- members (ADR-0017), also for a writer that skips forge-db (trigger, as for
-- group_members).
--
-- protected_refs: the rules of ADR-0008. The CHECKs hold the prefix and the
-- length (in bytes) of forge_core RefPattern::parse; the wildcard and
-- check-ref-format rules are the parser's alone. One pattern per project
-- ignoring ASCII case (the unique index on lower(pattern), pattern is
-- COLLATE "C" so lower() folds ASCII letters only, on every server).
--
-- No role gets TRUNCATE (it would bypass the row triggers); doctor compares
-- the exact ACLs (bootstrap/schema.rs).

CREATE TABLE forge.projects (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    display_name  text NOT NULL CONSTRAINT projects_display_name_length
        CHECK (length(display_name) BETWEEN 1 AND 255),
    description   text NOT NULL DEFAULT '' CONSTRAINT projects_description_length
        CHECK (length(description) <= 2000),
    visibility    text NOT NULL DEFAULT 'private' CONSTRAINT projects_visibility
        CHECK (visibility IN ('private', 'internal')),
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    deleted_at    timestamptz,
    purge_after   timestamptz,
    CONSTRAINT projects_deletion_pair CHECK (
        (deleted_at IS NULL) = (purge_after IS NULL)
        AND (purge_after IS NULL OR purge_after >= deleted_at))
);

CREATE TABLE forge.repositories (
    id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    project_id       bigint NOT NULL REFERENCES forge.projects (id),
    kind             text NOT NULL CONSTRAINT repositories_kind
        CHECK (kind IN ('spec', 'system')),
    state            text NOT NULL DEFAULT 'provisioning' CONSTRAINT repositories_state
        CHECK (state IN ('provisioning', 'ready')),
    provision_token  uuid NOT NULL DEFAULT gen_random_uuid(),
    created_at       timestamptz NOT NULL DEFAULT now(),
    ready_at         timestamptz,
    CONSTRAINT repositories_project_key UNIQUE (project_id),
    CONSTRAINT repositories_ready_at CHECK ((state = 'ready') = (ready_at IS NOT NULL))
);
-- Doctor and the provisioning report look for repositories still waiting.
CREATE INDEX repositories_provisioning ON forge.repositories (created_at)
    WHERE state = 'provisioning';

-- The state moves only forward: provisioning -> ready, which sets ready_at
-- once; ready_at never changes after that. A guard (XF001, FG-DB-0009
-- reason=rejected), enabled ALWAYS so it holds for every role; doctor
-- compares the trigger and the function body (bootstrap/schema.rs).
CREATE FUNCTION forge.repositories_state_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF (NEW.state IS DISTINCT FROM OLD.state
            AND NOT (OLD.state = 'provisioning' AND NEW.state = 'ready'))
       OR (OLD.ready_at IS NOT NULL AND NEW.ready_at IS DISTINCT FROM OLD.ready_at) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the state of a repository on forge.repositories moves only from provisioning to ready, once';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.repositories_state_guard() FROM PUBLIC;
CREATE TRIGGER repositories_state_forward BEFORE UPDATE ON forge.repositories
    FOR EACH ROW EXECUTE FUNCTION forge.repositories_state_guard();
ALTER TABLE forge.repositories ENABLE ALWAYS TRIGGER repositories_state_forward;

CREATE TABLE forge.project_members (
    project_id  bigint NOT NULL REFERENCES forge.projects (id),
    user_id     bigint NOT NULL REFERENCES forge.users (id),
    role        text NOT NULL CONSTRAINT project_members_role
        CHECK (role IN ('guest', 'reporter', 'developer', 'maintainer', 'owner')),
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT project_members_pkey PRIMARY KEY (project_id, user_id)
);
-- The role loader looks memberships up by user.
CREATE INDEX project_members_user ON forge.project_members (user_id);

CREATE FUNCTION forge.project_members_no_bot() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM forge.users WHERE id = NEW.user_id AND kind = 'bot') THEN
        RAISE EXCEPTION USING ERRCODE = 'check_violation', CONSTRAINT = 'project_members_no_bot',
            SCHEMA = TG_TABLE_SCHEMA, TABLE = TG_TABLE_NAME,
            MESSAGE = 'a bot cannot be a project member';
    END IF;
    RETURN NEW;
END
$$;
CREATE TRIGGER project_members_no_bot
    BEFORE INSERT OR UPDATE OF user_id ON forge.project_members
    FOR EACH ROW EXECUTE FUNCTION forge.project_members_no_bot();
ALTER TABLE forge.project_members ENABLE ALWAYS TRIGGER project_members_no_bot;

CREATE TABLE forge.protected_refs (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    project_id     bigint NOT NULL REFERENCES forge.projects (id),
    pattern        text COLLATE "C" NOT NULL
        CONSTRAINT protected_refs_pattern_prefix CHECK (pattern ~ '^refs/(heads|tags)/')
        CONSTRAINT protected_refs_pattern_length CHECK (octet_length(pattern) <= 255),
    push_min_role  text NOT NULL CONSTRAINT protected_refs_push_min_role
        CHECK (push_min_role IN ('developer', 'maintainer', 'owner')),
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now()
);
-- One rule per pattern ignoring case; also the lookup of a project's rules.
CREATE UNIQUE INDEX protected_refs_project_pattern
    ON forge.protected_refs (project_id, lower(pattern));

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, display_name, description, visibility, created_at,
        deleted_at, purge_after ON forge.projects
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'id', 'updated_at', '');
ALTER TABLE forge.projects ENABLE ALWAYS TRIGGER audited_change_present;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, project_id, kind, state, provision_token, created_at,
        ready_at ON forge.repositories
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.repositories ENABLE ALWAYS TRIGGER audited_change_present;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF project_id, user_id, role, created_at
    ON forge.project_members
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', 'updated_at', '');
ALTER TABLE forge.project_members ENABLE ALWAYS TRIGGER audited_change_present;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, project_id, pattern, push_min_role, created_at
    ON forge.protected_refs
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', 'updated_at', '');
ALTER TABLE forge.protected_refs ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT, INSERT, UPDATE ON forge.projects TO forge_app;
GRANT SELECT ON forge.projects TO forge_ro;
GRANT SELECT, INSERT ON forge.repositories TO forge_app;
GRANT UPDATE (state, ready_at) ON forge.repositories TO forge_app;
GRANT SELECT (id, project_id, kind, state, created_at, ready_at) ON forge.repositories TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.project_members TO forge_app;
GRANT SELECT ON forge.project_members TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.protected_refs TO forge_app;
GRANT SELECT ON forge.protected_refs TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.projects'),
    ('trigger', 'audited_change_present on forge.repositories'),
    ('trigger', 'audited_change_present on forge.project_members'),
    ('trigger', 'audited_change_present on forge.protected_refs'),
    ('function', 'forge.repositories_state_guard()'),
    ('trigger', 'repositories_state_forward on forge.repositories')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261031100000, "guarded_changes": ["forge.projects.audited_change_present", "forge.repositories.audited_change_present", "forge.project_members.audited_change_present", "forge.protected_refs.audited_change_present", "forge.repositories_state_guard", "forge.repositories.repositories_state_forward"]}');
