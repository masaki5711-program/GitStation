-- M3 C1 (ADR-0024 §10, §11; plan 02 §5.4, plan 11 §4.1): the Checks API.
--
-- 1. check_runs: one row per (project, commit, name) — reporting the same
--    name on the same commit again updates that row (no PATCH, no history
--    of reports: the audit log keeps who reported what). The name follows
--    forge_core::CheckName::parse exactly (check_runs_name: one of the
--    namespaces ci / forge / kanflow / ext, then 1–100 bytes of lowercase
--    letters, digits, `.`, `_`, `-` and `/`, starting with a letter or a
--    digit, no empty segment, no trailing `/`). status is queued /
--    in_progress / completed; conclusion (success / neutral / failure /
--    error / action_required) and completed_at exist exactly when
--    completed. summary is plain text / Markdown stored as written, at most
--    64 KiB; details_url an http(s) URL of at most 2 KiB (the server never
--    fetches it). reported_by is the account that wrote the row last
--    (with its token and request id). revision counts the row's writes
--    (1 at creation); the `check.completed` event of a write that changed
--    the conclusion is deduplicated by `check_completed:<id>:<revision>`.
--    At most 100 names per commit (forge_core::MAX_CHECK_RUNS_PER_COMMIT,
--    the application's FG-REVIEW-0021; every read of a commit's runs is
--    bounded by it). The guard check_runs_guard keeps a run's identity
--    (project, commit, name, creation) fixed and makes every change add one
--    to its revision.
--
-- 2. check_annotations: at most 50 per run (position 0..49, unique per
--    run), each on a relative path (no leading `/`, no `..` segment, no
--    backslash, no control character; 1–4096 bytes) and a line range,
--    with a level (notice / warning / failure) and a message (1 character
--    to 4 KiB). A report replaces the run's annotations as a whole.
--
-- 3. check_namespace_grants: which service account may report checks in
--    which namespace (ci / kanflow only: `forge/` is forge's own worker,
--    `ext/` is no service account's). The guard
--    check_namespace_grants_kind_guard refuses a grant to any other kind
--    of account (constraint name check_namespace_grants_service_account;
--    users.kind never changes, so checking on insert and on a changed
--    user_id suffices). Granted and revoked by administrators
--    (`check_namespace.granted` / `.revoked`, audit only, on the account);
--    a revoke deletes the row.
--
-- check_runs and check_annotations are audited state on the project
-- (resource `project`, id `project_id`): every report commits with a
-- `check_run.reported` row (or `check.completed`, with its event, when the
-- conclusion changed); the project purge's `project.purged` row covers
-- their deletion (forge_app may DELETE, projects/purge.rs: annotations
-- before runs). check_namespace_grants is audited on the account
-- (resource `user`, id `user_id`).

CREATE TABLE forge.check_runs (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    project_id    bigint NOT NULL REFERENCES forge.projects (id),
    head_sha      text NOT NULL CONSTRAINT check_runs_head_sha
        CHECK (head_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    name          text COLLATE "C" NOT NULL CONSTRAINT check_runs_name
        CHECK (name ~ '^(ci|forge|kanflow|ext)/(?=[a-z0-9._/-]{1,100}$)[a-z0-9][a-z0-9._-]*(/[a-z0-9._-]+)*$'),
    status        text NOT NULL CONSTRAINT check_runs_status
        CHECK (status IN ('queued', 'in_progress', 'completed')),
    conclusion    text CONSTRAINT check_runs_conclusion
        CHECK (conclusion IN ('success', 'neutral', 'failure', 'error', 'action_required')),
    summary       text NOT NULL DEFAULT '' CONSTRAINT check_runs_summary_length
        CHECK (octet_length(summary) <= 65536),
    details_url   text CONSTRAINT check_runs_details_url
        CHECK (octet_length(details_url) <= 2048
            AND details_url ~ '^https?://[^[:space:][:cntrl:]]+$'),
    reported_by   bigint NOT NULL REFERENCES forge.users (id),
    credential_id bigint,
    request_id    text NOT NULL CONSTRAINT check_runs_request_id_length
        CHECK (octet_length(request_id) BETWEEN 1 AND 128),
    revision      bigint NOT NULL DEFAULT 1 CONSTRAINT check_runs_revision_positive
        CHECK (revision > 0),
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    completed_at  timestamptz,
    CONSTRAINT check_runs_conclusion_when_completed CHECK (
        (status = 'completed') = (conclusion IS NOT NULL)
        AND (status = 'completed') = (completed_at IS NOT NULL)),
    CONSTRAINT check_runs_one_per_name UNIQUE (project_id, head_sha, name)
);

CREATE TABLE forge.check_annotations (
    check_run_id  bigint NOT NULL REFERENCES forge.check_runs (id),
    project_id    bigint NOT NULL REFERENCES forge.projects (id),
    position      smallint NOT NULL CONSTRAINT check_annotations_position
        CHECK (position BETWEEN 0 AND 49),
    path          text COLLATE "C" NOT NULL CONSTRAINT check_annotations_path
        CHECK (octet_length(path) BETWEEN 1 AND 4096
            AND path !~ '^/'
            AND path !~ '(^|/)\.\.(/|$)'
            AND path !~ '[\\[:cntrl:]]'),
    start_line    integer NOT NULL CONSTRAINT check_annotations_start_line
        CHECK (start_line >= 1),
    end_line      integer NOT NULL,
    level         text NOT NULL CONSTRAINT check_annotations_level
        CHECK (level IN ('notice', 'warning', 'failure')),
    message       text NOT NULL CONSTRAINT check_annotations_message_length
        CHECK (char_length(message) >= 1 AND octet_length(message) <= 4096),
    title         text CONSTRAINT check_annotations_title_length
        CHECK (octet_length(title) <= 255),
    CONSTRAINT check_annotations_lines CHECK (end_line >= start_line),
    PRIMARY KEY (check_run_id, position)
);
-- The project purge deletes a project's annotations.
CREATE INDEX check_annotations_project ON forge.check_annotations (project_id);

CREATE TABLE forge.check_namespace_grants (
    user_id     bigint NOT NULL REFERENCES forge.users (id),
    namespace   text NOT NULL CONSTRAINT check_namespace_grants_namespace
        CHECK (namespace IN ('ci', 'kanflow')),
    granted_by  bigint NOT NULL REFERENCES forge.users (id),
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, namespace)
);
-- The reporters of a namespace (doctor `checks.reporters`, the merge rule's
-- `unreportable_checks`).
CREATE INDEX check_namespace_grants_namespace ON forge.check_namespace_grants (namespace);

CREATE FUNCTION forge.check_runs_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NEW.project_id IS DISTINCT FROM OLD.project_id
       OR NEW.head_sha IS DISTINCT FROM OLD.head_sha
       OR NEW.name IS DISTINCT FROM OLD.name
       OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the identity of a check run on forge.check_runs never changes';
    END IF;
    IF NEW IS DISTINCT FROM OLD AND NEW.revision IS DISTINCT FROM OLD.revision + 1 THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'every change of a check run on forge.check_runs adds one to its revision';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.check_runs_guard() FROM PUBLIC;
CREATE TRIGGER check_runs_identity BEFORE UPDATE ON forge.check_runs
    FOR EACH ROW EXECUTE FUNCTION forge.check_runs_guard();
ALTER TABLE forge.check_runs ENABLE ALWAYS TRIGGER check_runs_identity;

CREATE FUNCTION forge.check_namespace_grants_kind_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM forge.users
                   WHERE id = NEW.user_id AND kind = 'service_account') THEN
        RAISE EXCEPTION USING ERRCODE = 'check_violation',
            CONSTRAINT = 'check_namespace_grants_service_account',
            SCHEMA = TG_TABLE_SCHEMA, TABLE = TG_TABLE_NAME,
            MESSAGE = 'a check namespace is granted to a service account only';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.check_namespace_grants_kind_guard() FROM PUBLIC;
CREATE TRIGGER check_namespace_grants_kind_guard
    BEFORE INSERT OR UPDATE OF user_id ON forge.check_namespace_grants
    FOR EACH ROW EXECUTE FUNCTION forge.check_namespace_grants_kind_guard();
ALTER TABLE forge.check_namespace_grants ENABLE ALWAYS TRIGGER check_namespace_grants_kind_guard;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, project_id, head_sha, name, status, conclusion,
        summary, details_url, reported_by, credential_id, request_id, revision, created_at,
        updated_at, completed_at
    ON forge.check_runs
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.check_runs ENABLE ALWAYS TRIGGER audited_change_present;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF check_run_id, project_id, position, path, start_line,
        end_line, level, message, title
    ON forge.check_annotations
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.check_annotations ENABLE ALWAYS TRIGGER audited_change_present;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF user_id, namespace, granted_by, created_at
    ON forge.check_namespace_grants
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('user', 'user_id', '', '');
ALTER TABLE forge.check_namespace_grants ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.check_runs TO forge_app;
GRANT SELECT ON forge.check_runs TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.check_annotations TO forge_app;
GRANT SELECT ON forge.check_annotations TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.check_namespace_grants TO forge_app;
GRANT SELECT ON forge.check_namespace_grants TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.check_runs'),
    ('trigger', 'audited_change_present on forge.check_annotations'),
    ('trigger', 'audited_change_present on forge.check_namespace_grants'),
    ('function', 'forge.check_runs_guard()'),
    ('trigger', 'check_runs_identity on forge.check_runs'),
    ('function', 'forge.check_namespace_grants_kind_guard()'),
    ('trigger', 'check_namespace_grants_kind_guard on forge.check_namespace_grants')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261115100000, "guarded_changes": ["forge.check_runs.audited_change_present", "forge.check_annotations.audited_change_present", "forge.check_namespace_grants.audited_change_present", "forge.check_runs_guard", "forge.check_runs.check_runs_identity", "forge.check_namespace_grants_kind_guard", "forge.check_namespace_grants.check_namespace_grants_kind_guard"]}');
