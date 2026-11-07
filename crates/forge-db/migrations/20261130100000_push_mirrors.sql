-- M3 O2 (plan 11 §5 O2, plan 06 D-2 / D-4 §4.11, ADR-0024 Amendment O2): the
-- push mirror of a repository to GitLab, the insurance copy.
--
-- 1. forge.push_mirrors (audited on ('project', project_id)): at most one
--    mirror per repository, set by an administrator (PushMirrorManage). The
--    URL passed forge-core's mirror target policy when stored (https, no
--    user information, query or fragment, a host of mirror.allowed_hosts);
--    the job checks it again against the policy of the moment. The token is
--    never in the database: `credential_name` names a file in
--    mirror.credentials_dir (mode 0600, outside the database, ADR-0013).
--    The read pool sees the origin (scheme://host:port), not the full URL.
--    `updated_at` is unaudited.
--
-- 2. forge.push_mirror_status (DERIVED / operational, tables.toml kind =
--    "derived"): one row per mirror, written by the `push-mirror`
--    subscriber (a push is waiting) and the `repository.push_mirror` job
--    (the result). Nothing reads it for an authorization decision; doctor
--    and the worker's lag sweep read it (FG-INT-0013 / FG-INT-0014).
--
--      pending_seq      the newest repository.pushed seen (0 = none)
--      pending_since    when the oldest push not yet mirrored arrived;
--                       NULL when the mirror is up to date
--      mirrored_seq     the pending_seq the last successful push covered
--      failures         consecutive failed attempts (0 after a success)
--      last_reason      the short reason of the last failure
--      lag_alerted_at   the last FG-INT-0014 (logged at most once an hour)
--
-- Both are deleted with the mirror (`DELETE …/push-mirror`, the status row
-- first) and by the project purge (projects/purge.rs, before the
-- repository row).

CREATE TABLE forge.push_mirrors (
    repo_id          bigint PRIMARY KEY REFERENCES forge.repositories (id),
    project_id       bigint NOT NULL REFERENCES forge.projects (id),
    url              text NOT NULL CONSTRAINT push_mirrors_url
        CHECK (length(url) BETWEEN 9 AND 2048 AND url ~ '^https?://[^/@?#[:space:]]+(/[^?#[:space:]]*)?$'),
    origin           text NOT NULL CONSTRAINT push_mirrors_origin
        CHECK (length(origin) BETWEEN 9 AND 300 AND origin ~ '^https?://[^/@?#[:space:]]+$'),
    credential_name  text NOT NULL CONSTRAINT push_mirrors_credential_name
        CHECK (credential_name ~ '^[a-z0-9][a-z0-9._-]{0,63}$'),
    enabled          boolean NOT NULL DEFAULT true,
    created_by       bigint NOT NULL REFERENCES forge.users (id),
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX push_mirrors_project ON forge.push_mirrors (project_id);

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF repo_id, project_id, url, origin, credential_name,
        enabled, created_by, created_at
    ON forge.push_mirrors
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', 'updated_at', '');
ALTER TABLE forge.push_mirrors ENABLE ALWAYS TRIGGER audited_change_present;

CREATE TABLE forge.push_mirror_status (
    repo_id          bigint PRIMARY KEY REFERENCES forge.push_mirrors (repo_id),
    project_id       bigint NOT NULL REFERENCES forge.projects (id),
    pending_seq      bigint NOT NULL DEFAULT 0
        CONSTRAINT push_mirror_status_pending_seq CHECK (pending_seq >= 0),
    pending_since    timestamptz,
    mirrored_seq     bigint NOT NULL DEFAULT 0
        CONSTRAINT push_mirror_status_mirrored_seq CHECK (mirrored_seq >= 0),
    last_attempt_at  timestamptz,
    last_success_at  timestamptz,
    failures         integer NOT NULL DEFAULT 0
        CONSTRAINT push_mirror_status_failures CHECK (failures >= 0),
    last_error_code  text CONSTRAINT push_mirror_status_error_code
        CHECK (last_error_code ~ '^FG-[A-Z]{2,8}-[0-9]{4}$'),
    last_reason      text CONSTRAINT push_mirror_status_reason
        CHECK (last_reason ~ '^[a-z][a-z0-9_]{0,39}$'),
    lag_alerted_at   timestamptz,
    updated_at       timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT push_mirror_status_failure_shape
        CHECK ((failures = 0) = (last_error_code IS NULL)),
    CONSTRAINT push_mirror_status_mirrored_le_pending
        CHECK (mirrored_seq <= pending_seq)
);
-- The lag sweep and doctor read the waiting mirrors, oldest first.
CREATE INDEX push_mirror_status_pending ON forge.push_mirror_status (pending_since)
    WHERE pending_since IS NOT NULL;
CREATE INDEX push_mirror_status_project ON forge.push_mirror_status (project_id);

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.push_mirrors TO forge_app;
GRANT SELECT (repo_id, project_id, origin, credential_name, enabled, created_by, created_at,
    updated_at) ON forge.push_mirrors TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.push_mirror_status TO forge_app;
GRANT SELECT ON forge.push_mirror_status TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.push_mirrors')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261130100000, "guarded_changes": ["forge.push_mirrors.audited_change_present"]}');
