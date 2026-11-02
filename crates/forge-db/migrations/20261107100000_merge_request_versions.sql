-- M3 R1b (ADR-0024 §1, §2; plan 11 §4.1, §11.3): merge request versions.
--
-- merge_request_versions: one row per head commit a merge request had
-- (version_no 1, 2, … per merge request; unique per merge request, and
-- one row per head_sha: the source branch going back to an older head
-- makes that version the head again, it adds none). The version job
-- (`merge_request.refresh`, forge-cli worker → forge-review) commits the
-- row FIRST and only then writes the hidden refs
-- `refs/merge-requests/<iid>/v<version_no>` and `…/head` from the
-- committed row (ADR-0024 §2: the number is never taken from a ref).
-- pushed_by is the pusher whose push triggered the version (NULL when no
-- push did: the first version of a new or reopened merge request).
-- base_sha / merge_base_sha are filled by R2's mergeability job (NULL until
-- then). refs_purged_at marks versions whose refs an administrator removed
-- (secret removal, MergeRequestVersionRefsPurge): the job never writes
-- those refs again (ADR-0024 closing table #42).
--
-- Audited state on the project (resource `project`, id `project_id`, like
-- merge_requests): the job writes `merge_request.pushed` (or the audit-only
-- `merge_request.head_restored`) with actor `system`, the purge
-- `merge_request.version_refs_purged`. forge_app may DELETE for the project
-- purge (projects/purge.rs).
--
-- The guard merge_request_versions_guard keeps a version's identity fixed
-- (merge request, project, number, head, pusher, creation time) and lets
-- base_sha, merge_base_sha and refs_purged_at be set once, never changed
-- or cleared.

CREATE TABLE forge.merge_request_versions (
    id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    merge_request_id  bigint NOT NULL REFERENCES forge.merge_requests (id),
    project_id        bigint NOT NULL REFERENCES forge.projects (id),
    version_no        bigint NOT NULL CONSTRAINT merge_request_versions_version_positive
        CHECK (version_no > 0),
    head_sha          text NOT NULL CONSTRAINT merge_request_versions_head_sha
        CHECK (head_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    base_sha          text CONSTRAINT merge_request_versions_base_sha
        CHECK (base_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    merge_base_sha    text CONSTRAINT merge_request_versions_merge_base_sha
        CHECK (merge_base_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    pushed_by         bigint REFERENCES forge.users (id),
    created_at        timestamptz NOT NULL DEFAULT now(),
    refs_purged_at    timestamptz,
    CONSTRAINT merge_request_versions_number UNIQUE (merge_request_id, version_no),
    CONSTRAINT merge_request_versions_head UNIQUE (merge_request_id, head_sha)
);
-- The project purge deletes a project's versions.
CREATE INDEX merge_request_versions_project ON forge.merge_request_versions (project_id);

CREATE FUNCTION forge.merge_request_versions_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NEW.merge_request_id IS DISTINCT FROM OLD.merge_request_id
       OR NEW.project_id IS DISTINCT FROM OLD.project_id
       OR NEW.version_no IS DISTINCT FROM OLD.version_no
       OR NEW.head_sha IS DISTINCT FROM OLD.head_sha
       OR NEW.pushed_by IS DISTINCT FROM OLD.pushed_by
       OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the identity of a version on forge.merge_request_versions never changes';
    END IF;
    IF (OLD.refs_purged_at IS NOT NULL AND NEW.refs_purged_at IS DISTINCT FROM OLD.refs_purged_at)
       OR (OLD.base_sha IS NOT NULL AND NEW.base_sha IS DISTINCT FROM OLD.base_sha)
       OR (OLD.merge_base_sha IS NOT NULL
           AND NEW.merge_base_sha IS DISTINCT FROM OLD.merge_base_sha) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'refs_purged_at, base_sha and merge_base_sha on forge.merge_request_versions are set once';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.merge_request_versions_guard() FROM PUBLIC;
CREATE TRIGGER merge_request_versions_set_once BEFORE UPDATE ON forge.merge_request_versions
    FOR EACH ROW EXECUTE FUNCTION forge.merge_request_versions_guard();
ALTER TABLE forge.merge_request_versions ENABLE ALWAYS TRIGGER merge_request_versions_set_once;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, merge_request_id, project_id, version_no, head_sha,
        base_sha, merge_base_sha, pushed_by, created_at, refs_purged_at
    ON forge.merge_request_versions
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.merge_request_versions ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.merge_request_versions TO forge_app;
GRANT SELECT ON forge.merge_request_versions TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.merge_request_versions'),
    ('function', 'forge.merge_request_versions_guard()'),
    ('trigger', 'merge_request_versions_set_once on forge.merge_request_versions')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261107100000, "guarded_changes": ["forge.merge_request_versions.audited_change_present", "forge.merge_request_versions_guard", "forge.merge_request_versions.merge_request_versions_set_once"]}');
