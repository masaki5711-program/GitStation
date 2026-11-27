-- Plan 17 P17-7 (ADR-0031 Amendment P17-7): `pfs-ai-bundle` releases.
--
-- forge.ai_bundle_releases (audited on ('project', project_id)): one signed
-- release of a PFS AI asset repository (a project whose ai_assets.enabled
-- is on), made by a person from a tag `assets/v<yyyy>.<ww>.<n>`:
--   commit_sha   the commit the tag named when the release was made (a
--                moved tag never changes it; the version and the commit
--                are each released once per project)
--   tag_object   the tag's object then (an annotated tag's own id)
--   manifest     the forge_agent::bundle manifest (files and their
--                SHA-256, models, the bench) as signed
--   digest       sha256:<hex> of the manifest (forge-pfs-ai-bundle-digest-v1)
--   key_id / signature  Ed25519 by the agent signing key (plan 16 A1) in
--                the domain forge-pfs-ai-bundle-signature-v1
--   bench_check_run_id  the `forge/ai-eval` run that concluded success
-- A release never changes and is never deleted (the guard; forge_app has
-- INSERT of the caller's columns and SELECT only). Like a published agent
-- part (ADR-0032 Amendment A3 §3) it has no foreign key to the project and
-- outlives its purge: the PFS loader keeps verifying what it installed.
-- The creation writes its audit row (`ai_asset_release.created`) in the
-- same transaction. Deployments of a release are ordinary
-- forge.deployments rows of its commit_sha.

CREATE TABLE forge.ai_bundle_releases (
    id                  bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    project_id          bigint NOT NULL,
    kind                text NOT NULL CONSTRAINT ai_bundle_releases_kind
        CHECK (kind = 'pfs-ai-bundle'),
    version             text NOT NULL CONSTRAINT ai_bundle_releases_version
        CHECK (version ~ '^v[1-9][0-9]{3}\.(0[1-9]|[1-4][0-9]|5[0-3])\.[1-9][0-9]{0,2}$'),
    tag                 text NOT NULL CONSTRAINT ai_bundle_releases_tag
        CHECK (tag = 'assets/' || version),
    tag_object          text NOT NULL CONSTRAINT ai_bundle_releases_tag_object
        CHECK (tag_object ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    commit_sha          text NOT NULL CONSTRAINT ai_bundle_releases_commit_sha
        CHECK (commit_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    manifest            jsonb NOT NULL CONSTRAINT ai_bundle_releases_manifest
        CHECK (jsonb_typeof(manifest) = 'object' AND octet_length(manifest::text) <= 2097152),
    digest              text NOT NULL CONSTRAINT ai_bundle_releases_digest
        CHECK (digest ~ '^sha256:[0-9a-f]{64}$'),
    key_id              text NOT NULL CONSTRAINT ai_bundle_releases_key_id
        CHECK (key_id ~ '^[1-9][0-9]{0,4}$'),
    signature           text NOT NULL CONSTRAINT ai_bundle_releases_signature
        CHECK (signature ~ '^[A-Za-z0-9+/]{86}==$'),
    bench_check_run_id  bigint NOT NULL,
    files               integer NOT NULL CONSTRAINT ai_bundle_releases_files
        CHECK (files BETWEEN 1 AND 2000),
    created_by          bigint NOT NULL REFERENCES forge.users (id),
    created_at          timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT ai_bundle_releases_one_version UNIQUE (project_id, version),
    CONSTRAINT ai_bundle_releases_one_commit UNIQUE (project_id, commit_sha)
);
CREATE INDEX ai_bundle_releases_created_by ON forge.ai_bundle_releases (created_by);

CREATE FUNCTION forge.ai_bundle_releases_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a release of forge.ai_bundle_releases is never deleted';
    END IF;
    RAISE EXCEPTION USING ERRCODE = 'XF001',
        MESSAGE = 'a release of forge.ai_bundle_releases never changes';
END
$$;
REVOKE ALL ON FUNCTION forge.ai_bundle_releases_guard() FROM PUBLIC;
CREATE TRIGGER ai_bundle_releases_forward BEFORE UPDATE OR DELETE ON forge.ai_bundle_releases
    FOR EACH ROW EXECUTE FUNCTION forge.ai_bundle_releases_guard();
ALTER TABLE forge.ai_bundle_releases ENABLE ALWAYS TRIGGER ai_bundle_releases_forward;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, project_id, kind, version, tag, tag_object,
        commit_sha, manifest, digest, key_id, signature, bench_check_run_id, files, created_by,
        created_at
    ON forge.ai_bundle_releases
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.ai_bundle_releases ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT ON forge.ai_bundle_releases TO forge_app;
GRANT INSERT (project_id, kind, version, tag, tag_object, commit_sha, manifest, digest, key_id,
    signature, bench_check_run_id, files, created_by) ON forge.ai_bundle_releases TO forge_app;
GRANT SELECT ON forge.ai_bundle_releases TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('function', 'forge.ai_bundle_releases_guard()'),
    ('trigger', 'ai_bundle_releases_forward on forge.ai_bundle_releases'),
    ('trigger', 'audited_change_present on forge.ai_bundle_releases')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20270115100000, "guarded_changes": ["forge.ai_bundle_releases_guard", "forge.ai_bundle_releases.ai_bundle_releases_forward", "forge.ai_bundle_releases.audited_change_present"]}');
