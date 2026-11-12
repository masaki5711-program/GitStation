-- M4 C6 (plan 14 §4.1, §9 #2; ADR-0013, ADR-0029 decision 8 and its C6
-- amendment): a project's CI variables — the secrets its jobs name in
-- `secrets:` of .forge-ci.yml.
--
-- forge.ci_variables (audited on ('project', project_id)): set and deleted
-- by a Maintainer (CiVariableManage: people only), audited in the same
-- transaction (`ci_variable.created` / `.updated` / `.deleted`; never the
-- value). Deleted with the project in its purge.
--   name            `[A-Z][A-Z0-9_]{0,62}`, unique per project; not one the
--                   runner sets itself (CI, PATH, HOME, HOSTNAME, FORGE_*,
--                   CI_*: forge_core::ci_secret::check_name, the CHECK is
--                   the backstop)
--   key_id / ciphertext
--                   the value sealed with AES-256-GCM under key `key_id` of
--                   `ci.secret_keys_file` (ADR-0013: the key is outside the
--                   database), `nonce | ciphertext | tag`, the associated
--                   data binding it to (project_id, name). The value is
--                   never returned by any route after it was written; the
--                   read pool never sees the ciphertext (ADR-0018).
--   protected_only  default true. Today every value is released only to a
--                   push pipeline of a branch protected at its creation
--                   (ci_pipelines.protected, ADR-0029 obligation 5),
--                   whatever this flag says; it is kept so a later ADR can
--                   widen `false` without a migration.
--   masked          default true: the runner and forge replace the value
--                   (and its common encodings) in the job's log before it
--                   is stored. A masked value is at least 8 bytes.
--   value_bytes     the value's length (shown with the metadata; the value
--                   is not).

CREATE TABLE forge.ci_variables (
    id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    project_id       bigint NOT NULL REFERENCES forge.projects (id),
    name             text NOT NULL CONSTRAINT ci_variables_name CHECK (
        name ~ '^[A-Z][A-Z0-9_]{0,62}$'
        AND name NOT IN ('CI', 'PATH', 'HOME', 'HOSTNAME')
        AND name NOT LIKE 'FORGE\_%' AND name NOT LIKE 'CI\_%'),
    key_id           smallint NOT NULL CONSTRAINT ci_variables_key_id CHECK (key_id >= 1),
    ciphertext       bytea NOT NULL CONSTRAINT ci_variables_ciphertext
        CHECK (octet_length(ciphertext) BETWEEN 29 AND 4124),
    value_bytes      integer NOT NULL CONSTRAINT ci_variables_value_bytes
        CHECK (value_bytes BETWEEN 1 AND 4096),
    protected_only   boolean NOT NULL DEFAULT true,
    masked           boolean NOT NULL DEFAULT true,
    created_by       bigint NOT NULL REFERENCES forge.users (id),
    updated_by       bigint NOT NULL REFERENCES forge.users (id),
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT ci_variables_masked_length CHECK (NOT masked OR value_bytes >= 8),
    CONSTRAINT ci_variables_one_name UNIQUE (project_id, name)
);
CREATE INDEX ci_variables_created_by ON forge.ci_variables (created_by);
CREATE INDEX ci_variables_updated_by ON forge.ci_variables (updated_by);

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, project_id, name, key_id, ciphertext, value_bytes,
        protected_only, masked, created_by, updated_by, created_at
    ON forge.ci_variables
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', 'updated_at', '');
ALTER TABLE forge.ci_variables ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.ci_variables TO forge_app;
-- Not ciphertext (ADR-0018: the read pool never sees secret material).
GRANT SELECT (id, project_id, name, key_id, value_bytes, protected_only, masked, created_by,
    updated_by, created_at, updated_at) ON forge.ci_variables TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.ci_variables')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261211100000, "guarded_changes": ["forge.ci_variables.audited_change_present"]}');
