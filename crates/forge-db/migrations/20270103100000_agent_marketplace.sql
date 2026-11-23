-- Plan 16 A3 (§3, ADR-0032 and its A3 amendment): the agent marketplace's
-- tables. Parts are made in git and frozen into these tables at
-- publication (A5): a published version never changes, except its
-- channel (beta -> stable) and its state (published -> revoked), both
-- audited. Nothing here is ever deleted except a project's builds and
-- lesson injections by the project purge.
--
-- forge.agent_signing_keys (audited on ('agent_signing_key', id)): the
-- PUBLIC half of each Ed25519 key the server signs parts with (the seed
-- stays in agent.signing_keys_file, ADR-0013; never in the database).
--   kid          the key file's key id, 1..=65535 as decimal text (the
--                lock's `key_id`; the highest one in the file signs)
--   public_key   base64 of the 32-byte public key
--   retired_at   signs nothing new; still verifies what it signed
--   revoked_at   its signatures are refused (FG-AGENT-0002 key_revoked)
-- kid, public_key and created_at never change; retired and revoked are
-- set once (guard agent_signing_keys_forward). Registered by the operator
-- CLI `forge admin agent-key` (A5), never over HTTP.
--
-- forge.agent_parts (audited on ('agent_part', id)): a part's identity.
--   slug               unique, forge-agent's slug rule
--   kind               skill / agent / rule / hook / command / mcp / adapter
--   source_project_id  the project whose `<slug>/v<version>` tags it is
--                      published from (no foreign key: a published part
--                      outlives a purge of its source project)
--   deprecated_at/_by  set once
-- Everything else never changes; never deleted (guard agent_parts_forward).
--
-- forge.agent_part_versions (audited on ('agent_part', part_id)): one
-- published version. (part_id, version) is unique: a version is never
-- published twice (FG-AGENT-0012, A5). Inserted as channel 'beta', state
-- 'published', signed by a key that is neither retired nor revoked, of a
-- part that is not deprecated. Afterwards only channel beta -> stable
-- (while published) and state published -> revoked (with reason, by, at)
-- change; a revoked version is final (guard agent_part_versions_forward).
-- txid is the inserting transaction (set by the guard, whatever the
-- caller sent): the files of a version are inserted in that transaction
-- only.
--   ci_pipeline_id  the pipeline that tested the tag's SHA (no foreign
--                   key: pipelines go with their project's purge)
--
-- forge.agent_part_files: the files of a version, APPEND-ONLY (ADR-0018):
-- inserted only in the transaction that inserted the version (its audit
-- row covers them), at most 200 files and 1 MiB of content per version,
-- sha256 = the content's SHA-256 (checked by the guard); never updated or
-- deleted (guard agent_part_files_append_only). forge_app holds INSERT
-- and SELECT only.
--
-- forge.agent_builds (derived): a builder run of a project (A7) — the
-- selection, the lock's digest, dry run or the merge request it opened.
-- forge.agent_lesson_injections (derived): a confirmed Kanflow lesson
-- injected into a project's rules (A10); idempotency_key is unique (no
-- second merge request for the same lesson). Both are deleted with their
-- project by the purge.

CREATE TABLE forge.agent_signing_keys (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    kid             text NOT NULL CONSTRAINT agent_signing_keys_kid
        CHECK (CASE WHEN kid ~ '^[1-9][0-9]{0,4}$' THEN kid::integer <= 65535 ELSE false END),
    public_key      text NOT NULL CONSTRAINT agent_signing_keys_public_key
        CHECK (public_key ~ '^[A-Za-z0-9+/]{43}=$'),
    created_at      timestamptz NOT NULL DEFAULT now(),
    retired_at      timestamptz,
    revoked_at      timestamptz,
    revoked_reason  text CONSTRAINT agent_signing_keys_revoked_reason
        CHECK (char_length(revoked_reason) BETWEEN 1 AND 500),
    CONSTRAINT agent_signing_keys_one_kid UNIQUE (kid),
    CONSTRAINT agent_signing_keys_one_public_key UNIQUE (public_key),
    CONSTRAINT agent_signing_keys_revocation CHECK (
        (revoked_at IS NULL) = (revoked_reason IS NULL))
);

CREATE TABLE forge.agent_parts (
    id                 bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    slug               text NOT NULL CONSTRAINT agent_parts_slug
        CHECK (slug ~ '^[a-z0-9]([a-z0-9-]{0,62}[a-z0-9])?$'),
    kind               text NOT NULL CONSTRAINT agent_parts_kind
        CHECK (kind IN ('skill', 'agent', 'rule', 'hook', 'command', 'mcp', 'adapter')),
    source_project_id  bigint NOT NULL,
    created_by         bigint NOT NULL REFERENCES forge.users (id),
    created_at         timestamptz NOT NULL DEFAULT now(),
    deprecated_at      timestamptz,
    deprecated_by      bigint REFERENCES forge.users (id),
    CONSTRAINT agent_parts_one_slug UNIQUE (slug),
    CONSTRAINT agent_parts_deprecation CHECK ((deprecated_at IS NULL) = (deprecated_by IS NULL))
);
CREATE INDEX agent_parts_source_project ON forge.agent_parts (source_project_id);
CREATE INDEX agent_parts_created_by ON forge.agent_parts (created_by);
CREATE INDEX agent_parts_deprecated_by ON forge.agent_parts (deprecated_by);

CREATE TABLE forge.agent_part_versions (
    id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    part_id          bigint NOT NULL REFERENCES forge.agent_parts (id),
    version          text NOT NULL CONSTRAINT agent_part_versions_version
        CHECK (version ~ '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$'
               AND octet_length(version) <= 64),
    channel          text NOT NULL CONSTRAINT agent_part_versions_channel
        CHECK (channel IN ('beta', 'stable')),
    source_sha       text NOT NULL CONSTRAINT agent_part_versions_source_sha
        CHECK (source_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    tag              text NOT NULL CONSTRAINT agent_part_versions_tag
        CHECK (octet_length(tag) BETWEEN 1 AND 255),
    manifest         jsonb NOT NULL CONSTRAINT agent_part_versions_manifest
        CHECK (jsonb_typeof(manifest) = 'object'),
    digest           text NOT NULL CONSTRAINT agent_part_versions_digest
        CHECK (digest ~ '^sha256:[0-9a-f]{64}$'),
    signature        text NOT NULL CONSTRAINT agent_part_versions_signature
        CHECK (signature ~ '^[A-Za-z0-9+/]{86}==$'),
    key_id           text NOT NULL REFERENCES forge.agent_signing_keys (kid),
    capabilities     text[] NOT NULL DEFAULT '{}' CONSTRAINT agent_part_versions_capabilities
        CHECK (capabilities <@ ARRAY['exec', 'network', 'permissions', 'fs-write']::text[]
               AND array_position(capabilities, NULL) IS NULL),
    published_by     bigint NOT NULL REFERENCES forge.users (id),
    ci_pipeline_id   bigint NOT NULL,
    state            text NOT NULL CONSTRAINT agent_part_versions_state
        CHECK (state IN ('published', 'revoked')),
    revoked_reason   text CONSTRAINT agent_part_versions_revoked_reason
        CHECK (char_length(revoked_reason) BETWEEN 1 AND 500),
    revoked_by       bigint REFERENCES forge.users (id),
    revoked_at       timestamptz,
    created_at       timestamptz NOT NULL DEFAULT now(),
    txid             xid8 NOT NULL DEFAULT pg_catalog.pg_current_xact_id(),
    CONSTRAINT agent_part_versions_one_version UNIQUE (part_id, version),
    CONSTRAINT agent_part_versions_revocation CHECK (
        (state = 'revoked') = (revoked_reason IS NOT NULL)
        AND (state = 'revoked') = (revoked_by IS NOT NULL)
        AND (state = 'revoked') = (revoked_at IS NOT NULL))
);
CREATE INDEX agent_part_versions_key ON forge.agent_part_versions (key_id);
CREATE INDEX agent_part_versions_published_by ON forge.agent_part_versions (published_by);
CREATE INDEX agent_part_versions_revoked_by ON forge.agent_part_versions (revoked_by);

CREATE TABLE forge.agent_part_files (
    version_id  bigint NOT NULL REFERENCES forge.agent_part_versions (id),
    path        text NOT NULL CONSTRAINT agent_part_files_path
        CHECK (octet_length(path) BETWEEN 1 AND 255),
    mode        text NOT NULL CONSTRAINT agent_part_files_mode
        CHECK (mode IN ('100644', '100755')),
    sha256      text NOT NULL CONSTRAINT agent_part_files_sha256
        CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    content     bytea NOT NULL CONSTRAINT agent_part_files_content
        CHECK (octet_length(content) <= 1048576),
    CONSTRAINT agent_part_files_pkey PRIMARY KEY (version_id, path)
);

CREATE TABLE forge.agent_builds (
    id                  bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    project_id          bigint NOT NULL REFERENCES forge.projects (id),
    requested_by        bigint NOT NULL REFERENCES forge.users (id),
    adapter_version_id  bigint REFERENCES forge.agent_part_versions (id),
    selection           jsonb NOT NULL CONSTRAINT agent_builds_selection
        CHECK (jsonb_typeof(selection) = 'array'),
    lock_digest         text CONSTRAINT agent_builds_lock_digest
        CHECK (lock_digest ~ '^sha256:[0-9a-f]{64}$'),
    dry_run             boolean NOT NULL,
    mr_id               bigint REFERENCES forge.merge_requests (id),
    state               text NOT NULL CONSTRAINT agent_builds_state
        CHECK (state IN ('requested', 'succeeded', 'failed')),
    error_code          text CONSTRAINT agent_builds_error_code
        CHECK (error_code ~ '^FG-[A-Z]+-[0-9]{4}$'),
    request_id          text NOT NULL CONSTRAINT agent_builds_request_id
        CHECK (char_length(request_id) BETWEEN 1 AND 128),
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT agent_builds_dry_run_no_mr CHECK (NOT dry_run OR mr_id IS NULL),
    CONSTRAINT agent_builds_error CHECK ((state = 'failed') = (error_code IS NOT NULL))
);
CREATE INDEX agent_builds_project ON forge.agent_builds (project_id, id);
CREATE INDEX agent_builds_requested_by ON forge.agent_builds (requested_by);
CREATE INDEX agent_builds_adapter_version ON forge.agent_builds (adapter_version_id);
CREATE INDEX agent_builds_mr ON forge.agent_builds (mr_id);

CREATE TABLE forge.agent_lesson_injections (
    id                 bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    project_id         bigint NOT NULL REFERENCES forge.projects (id),
    kanflow_lesson_id  text NOT NULL CONSTRAINT agent_lesson_injections_lesson
        CHECK (char_length(kanflow_lesson_id) BETWEEN 1 AND 128),
    idempotency_key    text NOT NULL CONSTRAINT agent_lesson_injections_idempotency_key
        CHECK (char_length(idempotency_key) BETWEEN 1 AND 128),
    text_digest        text NOT NULL CONSTRAINT agent_lesson_injections_text_digest
        CHECK (text_digest ~ '^sha256:[0-9a-f]{64}$'),
    confirmed_by       bigint NOT NULL REFERENCES forge.users (id),
    mr_id              bigint REFERENCES forge.merge_requests (id),
    state              text NOT NULL CONSTRAINT agent_lesson_injections_state
        CHECK (state IN ('requested', 'mr_opened', 'failed')),
    error_code         text CONSTRAINT agent_lesson_injections_error_code
        CHECK (error_code ~ '^FG-[A-Z]+-[0-9]{4}$'),
    request_id         text NOT NULL CONSTRAINT agent_lesson_injections_request_id
        CHECK (char_length(request_id) BETWEEN 1 AND 128),
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT agent_lesson_injections_one_key UNIQUE (idempotency_key),
    CONSTRAINT agent_lesson_injections_error CHECK ((state = 'failed') = (error_code IS NOT NULL)),
    CONSTRAINT agent_lesson_injections_opened CHECK ((state = 'mr_opened') = (mr_id IS NOT NULL))
);
CREATE INDEX agent_lesson_injections_project ON forge.agent_lesson_injections (project_id, id);
CREATE INDEX agent_lesson_injections_confirmed_by ON forge.agent_lesson_injections (confirmed_by);
CREATE INDEX agent_lesson_injections_mr ON forge.agent_lesson_injections (mr_id);

-- ── Guards ──

CREATE FUNCTION forge.agent_signing_keys_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a key of forge.agent_signing_keys is never deleted';
    END IF;
    IF NEW.id IS DISTINCT FROM OLD.id
       OR NEW.kid IS DISTINCT FROM OLD.kid
       OR NEW.public_key IS DISTINCT FROM OLD.public_key
       OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the kid and public key of forge.agent_signing_keys never change';
    END IF;
    IF (OLD.retired_at IS NOT NULL AND NEW.retired_at IS DISTINCT FROM OLD.retired_at)
       OR (OLD.revoked_at IS NOT NULL AND (NEW.revoked_at IS DISTINCT FROM OLD.revoked_at
           OR NEW.revoked_reason IS DISTINCT FROM OLD.revoked_reason)) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the retirement and revocation of forge.agent_signing_keys are set once';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.agent_signing_keys_guard() FROM PUBLIC;
CREATE TRIGGER agent_signing_keys_forward BEFORE UPDATE OR DELETE ON forge.agent_signing_keys
    FOR EACH ROW EXECUTE FUNCTION forge.agent_signing_keys_guard();
ALTER TABLE forge.agent_signing_keys ENABLE ALWAYS TRIGGER agent_signing_keys_forward;

CREATE FUNCTION forge.agent_parts_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a part of forge.agent_parts is never deleted';
    END IF;
    IF NEW.id IS DISTINCT FROM OLD.id
       OR NEW.slug IS DISTINCT FROM OLD.slug
       OR NEW.kind IS DISTINCT FROM OLD.kind
       OR NEW.source_project_id IS DISTINCT FROM OLD.source_project_id
       OR NEW.created_by IS DISTINCT FROM OLD.created_by
       OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the identity of a part on forge.agent_parts never changes';
    END IF;
    IF OLD.deprecated_at IS NOT NULL AND (NEW.deprecated_at IS DISTINCT FROM OLD.deprecated_at
           OR NEW.deprecated_by IS DISTINCT FROM OLD.deprecated_by) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the deprecation of a part on forge.agent_parts is set once';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.agent_parts_guard() FROM PUBLIC;
CREATE TRIGGER agent_parts_forward BEFORE UPDATE OR DELETE ON forge.agent_parts
    FOR EACH ROW EXECUTE FUNCTION forge.agent_parts_guard();
ALTER TABLE forge.agent_parts ENABLE ALWAYS TRIGGER agent_parts_forward;

CREATE FUNCTION forge.agent_part_versions_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a version of forge.agent_part_versions is never deleted';
    END IF;
    IF TG_OP = 'INSERT' THEN
        NEW.txid := pg_catalog.pg_current_xact_id();
        IF NEW.channel <> 'beta' OR NEW.state <> 'published' THEN
            RAISE EXCEPTION USING ERRCODE = 'XF001',
                MESSAGE = 'a version of forge.agent_part_versions is published to beta first';
        END IF;
        IF NOT EXISTS (SELECT 1 FROM forge.agent_signing_keys AS k
                       WHERE k.kid = NEW.key_id AND k.retired_at IS NULL
                         AND k.revoked_at IS NULL) THEN
            RAISE EXCEPTION USING ERRCODE = 'XF001',
                MESSAGE = 'a version of forge.agent_part_versions is signed by a current key';
        END IF;
        IF NOT EXISTS (SELECT 1 FROM forge.agent_parts AS p
                       WHERE p.id = NEW.part_id AND p.deprecated_at IS NULL) THEN
            RAISE EXCEPTION USING ERRCODE = 'XF001',
                MESSAGE = 'a deprecated part of forge.agent_parts gets no new version';
        END IF;
        RETURN NEW;
    END IF;
    IF NEW.id IS DISTINCT FROM OLD.id
       OR NEW.part_id IS DISTINCT FROM OLD.part_id
       OR NEW.version IS DISTINCT FROM OLD.version
       OR NEW.source_sha IS DISTINCT FROM OLD.source_sha
       OR NEW.tag IS DISTINCT FROM OLD.tag
       OR NEW.manifest IS DISTINCT FROM OLD.manifest
       OR NEW.digest IS DISTINCT FROM OLD.digest
       OR NEW.signature IS DISTINCT FROM OLD.signature
       OR NEW.key_id IS DISTINCT FROM OLD.key_id
       OR NEW.capabilities IS DISTINCT FROM OLD.capabilities
       OR NEW.published_by IS DISTINCT FROM OLD.published_by
       OR NEW.ci_pipeline_id IS DISTINCT FROM OLD.ci_pipeline_id
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
       OR NEW.txid IS DISTINCT FROM OLD.txid THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a published version of forge.agent_part_versions never changes';
    END IF;
    IF OLD.state = 'revoked' AND NEW IS DISTINCT FROM OLD THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a revoked version of forge.agent_part_versions never changes';
    END IF;
    IF NEW.channel IS DISTINCT FROM OLD.channel
       AND NOT (OLD.channel = 'beta' AND NEW.channel = 'stable' AND NEW.state = 'published') THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a version of forge.agent_part_versions only moves from beta to stable';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.agent_part_versions_guard() FROM PUBLIC;
CREATE TRIGGER agent_part_versions_forward BEFORE INSERT OR UPDATE OR DELETE
    ON forge.agent_part_versions
    FOR EACH ROW EXECUTE FUNCTION forge.agent_part_versions_guard();
ALTER TABLE forge.agent_part_versions ENABLE ALWAYS TRIGGER agent_part_versions_forward;

CREATE FUNCTION forge.agent_part_files_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP <> 'INSERT' THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a file of forge.agent_part_files is never changed or deleted';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM forge.agent_part_versions AS v
                   WHERE v.id = NEW.version_id
                     AND v.txid = pg_catalog.pg_current_xact_id()) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the files of forge.agent_part_files are inserted with their version only';
    END IF;
    IF NEW.sha256 IS DISTINCT FROM pg_catalog.encode(pg_catalog.sha256(NEW.content), 'hex') THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the sha256 of a file of forge.agent_part_files is its content''s';
    END IF;
    IF (SELECT count(*) FROM forge.agent_part_files AS f WHERE f.version_id = NEW.version_id) >= 200
       OR (SELECT coalesce(sum(pg_catalog.octet_length(f.content)), 0)
           FROM forge.agent_part_files AS f WHERE f.version_id = NEW.version_id)
          + pg_catalog.octet_length(NEW.content) > 1048576 THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a version of forge.agent_part_files holds at most 200 files and 1 MiB';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.agent_part_files_guard() FROM PUBLIC;
CREATE TRIGGER agent_part_files_append_only BEFORE INSERT OR UPDATE OR DELETE
    ON forge.agent_part_files
    FOR EACH ROW EXECUTE FUNCTION forge.agent_part_files_guard();
ALTER TABLE forge.agent_part_files ENABLE ALWAYS TRIGGER agent_part_files_append_only;

-- ── Audit triggers (tables.toml kind = "audited") ──

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, kid, public_key, created_at, retired_at, revoked_at,
        revoked_reason
    ON forge.agent_signing_keys
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('agent_signing_key', 'id', '', '');
ALTER TABLE forge.agent_signing_keys ENABLE ALWAYS TRIGGER audited_change_present;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, slug, kind, source_project_id, created_by, created_at,
        deprecated_at, deprecated_by
    ON forge.agent_parts
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('agent_part', 'id', '', '');
ALTER TABLE forge.agent_parts ENABLE ALWAYS TRIGGER audited_change_present;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, part_id, version, channel, source_sha, tag, manifest,
        digest, signature, key_id, capabilities, published_by, ci_pipeline_id, state,
        revoked_reason, revoked_by, revoked_at, created_at, txid
    ON forge.agent_part_versions
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('agent_part', 'part_id', '', '');
ALTER TABLE forge.agent_part_versions ENABLE ALWAYS TRIGGER audited_change_present;

-- ── Privileges: no DELETE on the published tables; UPDATE of the
-- columns that may change only ──

GRANT SELECT, INSERT ON forge.agent_signing_keys TO forge_app;
GRANT UPDATE (retired_at, revoked_at, revoked_reason) ON forge.agent_signing_keys TO forge_app;
GRANT SELECT ON forge.agent_signing_keys TO forge_ro;
GRANT SELECT, INSERT ON forge.agent_parts TO forge_app;
GRANT UPDATE (deprecated_at, deprecated_by) ON forge.agent_parts TO forge_app;
GRANT SELECT ON forge.agent_parts TO forge_ro;
GRANT SELECT, INSERT ON forge.agent_part_versions TO forge_app;
GRANT UPDATE (channel, state, revoked_reason, revoked_by, revoked_at)
    ON forge.agent_part_versions TO forge_app;
GRANT SELECT ON forge.agent_part_versions TO forge_ro;
GRANT SELECT, INSERT ON forge.agent_part_files TO forge_app;
GRANT SELECT ON forge.agent_part_files TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.agent_builds TO forge_app;
GRANT SELECT ON forge.agent_builds TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.agent_lesson_injections TO forge_app;
GRANT SELECT ON forge.agent_lesson_injections TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('function', 'forge.agent_signing_keys_guard()'),
    ('trigger', 'agent_signing_keys_forward on forge.agent_signing_keys'),
    ('trigger', 'audited_change_present on forge.agent_signing_keys'),
    ('function', 'forge.agent_parts_guard()'),
    ('trigger', 'agent_parts_forward on forge.agent_parts'),
    ('trigger', 'audited_change_present on forge.agent_parts'),
    ('function', 'forge.agent_part_versions_guard()'),
    ('trigger', 'agent_part_versions_forward on forge.agent_part_versions'),
    ('trigger', 'audited_change_present on forge.agent_part_versions'),
    ('function', 'forge.agent_part_files_guard()'),
    ('trigger', 'agent_part_files_append_only on forge.agent_part_files')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20270103100000, "guarded_changes": ["forge.agent_signing_keys_guard", "forge.agent_signing_keys.agent_signing_keys_forward", "forge.agent_signing_keys.audited_change_present", "forge.agent_parts_guard", "forge.agent_parts.agent_parts_forward", "forge.agent_parts.audited_change_present", "forge.agent_part_versions_guard", "forge.agent_part_versions.agent_part_versions_forward", "forge.agent_part_versions.audited_change_present", "forge.agent_part_files_guard", "forge.agent_part_files.agent_part_files_append_only"]}');
