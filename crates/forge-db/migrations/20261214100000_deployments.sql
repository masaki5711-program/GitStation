-- M4 C11 (plan 14 §4.1, ADR-0031): deployments over SSH from forge worker.
--
-- forge.deploy_environments (audited on ('project', project_id)): one per
-- tier per project, set by a Maintainer (DeployEnvironmentManage, people
-- only), audited in the same transaction (`deploy_environment.created` /
-- `.updated`).
--   requires_approval     a person's DeployApprove before the worker runs a
--                         deployment; always for prod (CHECK)
--   approval_ttl_minutes  how long a request waits for an approval, and an
--                         approval for the worker (the database clock)
--   auto_rollback         roll every target back to the previous version
--                         when a deployment fails; never for prod (CHECK:
--                         a person decides, plan 04 §8)
--   auto_deploy_ref       the worker requests a deployment of the newest
--                         succeeded push pipeline of this branch; never for
--                         prod (CHECK)
--   freeze_windows        no deployment is requested or run while now() is
--                         inside one (checked at request and at execution)
--
-- forge.deploy_targets (audited on ('project', project_id)): the hosts of an
-- environment, set by an administrator (DeployTargetManage, people only):
-- host, port, login_user, the pinned host public key (type and base64, a
-- known_hosts line's two last fields) and the registered script name, the
-- only command run on the host (`<script> deploy|health|rollback <sha>`).
-- Never a private key: forge's key is deploy.ssh_key_file (ADR-0013).
--
-- forge.deployments (derived): one request of one SHA to one environment.
-- The request and the approval write their audit rows (`deployment.requested`,
-- `deployment.approved`) in the same transaction; the worker's progress is
-- the structured log. The guard deployments_forward keeps the identity, the
-- approval (set once) and the state machine (forge_core::deploy::
-- DeploymentStatus::may_become). At most one running deployment per
-- environment. previous_sha is the environment's last succeeded SHA,
-- recorded when the worker takes the deployment (the rollback target).
-- All three are deleted with the project in its purge.

CREATE TABLE forge.deploy_environments (
    id                    bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    project_id            bigint NOT NULL REFERENCES forge.projects (id),
    tier                  text NOT NULL CONSTRAINT deploy_environments_tier
        CHECK (tier IN ('dev', 'stg', 'prod')),
    requires_approval     boolean NOT NULL,
    approval_ttl_minutes  integer NOT NULL DEFAULT 60 CONSTRAINT deploy_environments_ttl
        CHECK (approval_ttl_minutes BETWEEN 1 AND 1440),
    auto_rollback         boolean NOT NULL,
    auto_deploy_ref       text CONSTRAINT deploy_environments_auto_deploy_ref
        CHECK (auto_deploy_ref ~ '^refs/heads/.' AND octet_length(auto_deploy_ref) <= 255),
    freeze_windows        tstzmultirange NOT NULL DEFAULT '{}',
    created_by            bigint NOT NULL REFERENCES forge.users (id),
    updated_by            bigint NOT NULL REFERENCES forge.users (id),
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT deploy_environments_prod_is_human CHECK (
        tier <> 'prod' OR (requires_approval AND NOT auto_rollback AND auto_deploy_ref IS NULL)),
    CONSTRAINT deploy_environments_one_tier UNIQUE (project_id, tier)
);
CREATE INDEX deploy_environments_created_by ON forge.deploy_environments (created_by);
CREATE INDEX deploy_environments_updated_by ON forge.deploy_environments (updated_by);
CREATE INDEX deploy_environments_auto ON forge.deploy_environments (id)
    WHERE auto_deploy_ref IS NOT NULL;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, project_id, tier, requires_approval,
        approval_ttl_minutes, auto_rollback, auto_deploy_ref, freeze_windows, created_by,
        updated_by, created_at
    ON forge.deploy_environments
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', 'updated_at', '');
ALTER TABLE forge.deploy_environments ENABLE ALWAYS TRIGGER audited_change_present;

CREATE TABLE forge.deploy_targets (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    environment_id  bigint NOT NULL REFERENCES forge.deploy_environments (id),
    project_id      bigint NOT NULL REFERENCES forge.projects (id),
    name            text NOT NULL CONSTRAINT deploy_targets_name
        CHECK (name ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    host            text NOT NULL CONSTRAINT deploy_targets_host
        CHECK (host ~ '^[a-z0-9]([a-z0-9.-]{0,251}[a-z0-9])?$' AND host NOT LIKE '%..%'),
    port            integer NOT NULL CONSTRAINT deploy_targets_port CHECK (port BETWEEN 1 AND 65535),
    login_user      text NOT NULL CONSTRAINT deploy_targets_login_user
        CHECK (login_user ~ '^[a-z_][a-z0-9_-]{0,31}$'),
    host_key_type   text NOT NULL CONSTRAINT deploy_targets_host_key_type CHECK (host_key_type IN
        ('ssh-ed25519', 'ecdsa-sha2-nistp256', 'ecdsa-sha2-nistp384', 'ecdsa-sha2-nistp521',
         'rsa-sha2-512', 'rsa-sha2-256')),
    host_key        text NOT NULL CONSTRAINT deploy_targets_host_key
        CHECK (host_key ~ '^[A-Za-z0-9+/]+={0,2}$' AND octet_length(host_key) <= 2800),
    host_key_fingerprint text NOT NULL CONSTRAINT deploy_targets_fingerprint
        CHECK (host_key_fingerprint ~ '^SHA256:[A-Za-z0-9+/]{43}$'),
    script          text NOT NULL CONSTRAINT deploy_targets_script
        CHECK (script ~ '^[a-z0-9][a-z0-9._-]{0,63}$'),
    enabled         boolean NOT NULL DEFAULT true,
    created_by      bigint NOT NULL REFERENCES forge.users (id),
    updated_by      bigint NOT NULL REFERENCES forge.users (id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT deploy_targets_one_name UNIQUE (environment_id, name)
);
CREATE INDEX deploy_targets_project ON forge.deploy_targets (project_id);
CREATE INDEX deploy_targets_created_by ON forge.deploy_targets (created_by);
CREATE INDEX deploy_targets_updated_by ON forge.deploy_targets (updated_by);

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, environment_id, project_id, name, host, port,
        login_user, host_key_type, host_key, host_key_fingerprint, script, enabled, created_by,
        updated_by, created_at
    ON forge.deploy_targets
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', 'updated_at', '');
ALTER TABLE forge.deploy_targets ENABLE ALWAYS TRIGGER audited_change_present;

CREATE TABLE forge.deployments (
    id                    bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    project_id            bigint NOT NULL REFERENCES forge.projects (id),
    environment_id        bigint NOT NULL REFERENCES forge.deploy_environments (id),
    tier                  text NOT NULL CONSTRAINT deployments_tier
        CHECK (tier IN ('dev', 'stg', 'prod')),
    sha                   text NOT NULL CONSTRAINT deployments_sha
        CHECK (sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    status                text NOT NULL CONSTRAINT deployments_status CHECK (status IN
        ('pending_approval', 'queued', 'running', 'succeeded', 'failed', 'rolled_back',
         'rollback_failed', 'blocked')),
    -- NULL: the system (an automatic deployment).
    requested_by          bigint REFERENCES forge.users (id),
    request_id            text NOT NULL CONSTRAINT deployments_request_id
        CHECK (char_length(request_id) BETWEEN 1 AND 128),
    merge_request_author  bigint REFERENCES forge.users (id),
    approved_by           bigint REFERENCES forge.users (id),
    approved_at           timestamptz,
    approval_expires_at   timestamptz,
    previous_sha          text CONSTRAINT deployments_previous_sha
        CHECK (previous_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    error_code            text CONSTRAINT deployments_error_code
        CHECK (error_code ~ '^FG-[A-Z]+-[0-9]{4}$'),
    failed_target         text CONSTRAINT deployments_failed_target
        CHECK (failed_target ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    created_at            timestamptz NOT NULL DEFAULT now(),
    started_at            timestamptz,
    finished_at           timestamptz,
    updated_at            timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT deployments_approval_fields CHECK (
        (approved_by IS NULL) = (approved_at IS NULL)
        AND (approved_by IS NULL) = (approval_expires_at IS NULL)),
    CONSTRAINT deployments_not_self_approved CHECK (
        approved_by IS NULL OR (approved_by IS DISTINCT FROM requested_by
            AND approved_by IS DISTINCT FROM merge_request_author)),
    CONSTRAINT deployments_pending_unapproved CHECK (
        status <> 'pending_approval' OR approved_by IS NULL),
    CONSTRAINT deployments_prod_approved CHECK (
        tier <> 'prod' OR status IN ('pending_approval', 'blocked') OR approved_by IS NOT NULL),
    CONSTRAINT deployments_started CHECK (
        (status IN ('pending_approval', 'queued', 'blocked')) = (started_at IS NULL)),
    CONSTRAINT deployments_finished CHECK (
        (status IN ('succeeded', 'failed', 'rolled_back', 'rollback_failed', 'blocked'))
            = (finished_at IS NOT NULL)),
    CONSTRAINT deployments_error CHECK (
        (status IN ('failed', 'rolled_back', 'rollback_failed', 'blocked'))
            = (error_code IS NOT NULL))
);
CREATE INDEX deployments_project ON forge.deployments (project_id, id);
CREATE INDEX deployments_environment ON forge.deployments (environment_id, id);
CREATE INDEX deployments_waiting ON forge.deployments (id)
    WHERE status IN ('pending_approval', 'queued');
CREATE UNIQUE INDEX deployments_one_running ON forge.deployments (environment_id)
    WHERE status = 'running';
CREATE INDEX deployments_requested_by ON forge.deployments (requested_by);
CREATE INDEX deployments_approved_by ON forge.deployments (approved_by);
CREATE INDEX deployments_merge_request_author ON forge.deployments (merge_request_author);

CREATE FUNCTION forge.deployments_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NEW.project_id IS DISTINCT FROM OLD.project_id
       OR NEW.environment_id IS DISTINCT FROM OLD.environment_id
       OR NEW.tier IS DISTINCT FROM OLD.tier
       OR NEW.sha IS DISTINCT FROM OLD.sha
       OR NEW.requested_by IS DISTINCT FROM OLD.requested_by
       OR NEW.request_id IS DISTINCT FROM OLD.request_id
       OR NEW.merge_request_author IS DISTINCT FROM OLD.merge_request_author
       OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the identity of a deployment on forge.deployments never changes';
    END IF;
    IF (OLD.approved_by IS NOT NULL AND (NEW.approved_by IS DISTINCT FROM OLD.approved_by
            OR NEW.approved_at IS DISTINCT FROM OLD.approved_at
            OR NEW.approval_expires_at IS DISTINCT FROM OLD.approval_expires_at))
       OR (OLD.previous_sha IS NOT NULL AND NEW.previous_sha IS DISTINCT FROM OLD.previous_sha)
       OR (OLD.started_at IS NOT NULL AND NEW.started_at IS DISTINCT FROM OLD.started_at) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the approval, previous_sha and started_at of a deployment on forge.deployments are set once';
    END IF;
    IF NEW.status IS DISTINCT FROM OLD.status AND NOT (
           (OLD.status = 'pending_approval' AND NEW.status IN ('queued', 'blocked'))
           OR (OLD.status = 'queued' AND NEW.status IN ('running', 'blocked'))
           OR (OLD.status = 'running'
               AND NEW.status IN ('succeeded', 'failed', 'rolled_back', 'rollback_failed'))) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a deployment on forge.deployments only moves forward, and a final state never changes';
    END IF;
    IF OLD.status IN ('succeeded', 'failed', 'rolled_back', 'rollback_failed', 'blocked')
       AND NEW IS DISTINCT FROM OLD THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a finished deployment on forge.deployments never changes';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.deployments_guard() FROM PUBLIC;
CREATE TRIGGER deployments_forward BEFORE UPDATE ON forge.deployments
    FOR EACH ROW EXECUTE FUNCTION forge.deployments_guard();
ALTER TABLE forge.deployments ENABLE ALWAYS TRIGGER deployments_forward;

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.deploy_environments TO forge_app;
GRANT SELECT ON forge.deploy_environments TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.deploy_targets TO forge_app;
GRANT SELECT ON forge.deploy_targets TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.deployments TO forge_app;
GRANT SELECT ON forge.deployments TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.deploy_environments'),
    ('trigger', 'audited_change_present on forge.deploy_targets'),
    ('function', 'forge.deployments_guard()'),
    ('trigger', 'deployments_forward on forge.deployments')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261214100000, "guarded_changes": ["forge.deploy_environments.audited_change_present", "forge.deploy_targets.audited_change_present", "forge.deployments_guard", "forge.deployments.deployments_forward"]}');
