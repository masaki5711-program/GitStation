-- M3 R5 re-review round 2 (ADR-0024 §8.3, "R5 の再レビュー（2 回目）"): the
-- committer rule must see every ref update of a push. The `repository.pushed`
-- audit row lists at most 100 refs within its byte budget (ADR-0016 §5,
-- `refs_truncated`); a push of more refs left the rest invisible, so a
-- pusher whose unlisted ref carried a merge request's commit escaped the rule.
--
-- push_unlisted_refs: the ref updates a push record does not list, written by
-- the record (repositories::record) in the same transaction as its audit row,
-- only when the list was cut. The new commit and the pusher (the replayed
-- pusher for a runbook replay) are what the rule needs; the request id ties a
-- row to its audit row. Audited on the project (the push record's own audit
-- row is the change), never updated (guard push_unlisted_refs_immutable),
-- deleted only by the project purge (projects/purge.rs). A record written
-- before this migration with `refs_truncated` and no rows here still makes the
-- committers unknown (fail closed).

CREATE TABLE forge.push_unlisted_refs (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    project_id     bigint NOT NULL REFERENCES forge.projects (id),
    repository_id  bigint NOT NULL,
    request_id     text NOT NULL CONSTRAINT push_unlisted_refs_request_id
        CHECK (char_length(request_id) BETWEEN 1 AND 128),
    pusher_id      bigint REFERENCES forge.users (id),
    ref_name       text COLLATE "C" NOT NULL CONSTRAINT push_unlisted_refs_ref_name
        CHECK (ref_name ~ '^refs/.' AND octet_length(ref_name) <= 1024),
    new_sha        text NOT NULL CONSTRAINT push_unlisted_refs_new_sha
        CHECK (new_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    created_at     timestamptz NOT NULL DEFAULT now()
);
-- The committer rule reads a repository's rows; the purge a project's.
CREATE INDEX push_unlisted_refs_repository ON forge.push_unlisted_refs (project_id, repository_id, id);
CREATE INDEX push_unlisted_refs_request ON forge.push_unlisted_refs (request_id);

CREATE FUNCTION forge.push_unlisted_refs_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    RAISE EXCEPTION USING ERRCODE = 'XF001',
        MESSAGE = 'a row of forge.push_unlisted_refs is never changed';
END
$$;
REVOKE ALL ON FUNCTION forge.push_unlisted_refs_guard() FROM PUBLIC;
CREATE TRIGGER push_unlisted_refs_immutable BEFORE UPDATE ON forge.push_unlisted_refs
    FOR EACH ROW EXECUTE FUNCTION forge.push_unlisted_refs_guard();
ALTER TABLE forge.push_unlisted_refs ENABLE ALWAYS TRIGGER push_unlisted_refs_immutable;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, project_id, repository_id, request_id, pusher_id,
        ref_name, new_sha, created_at
    ON forge.push_unlisted_refs
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.push_unlisted_refs ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.push_unlisted_refs TO forge_app;
GRANT SELECT ON forge.push_unlisted_refs TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.push_unlisted_refs'),
    ('function', 'forge.push_unlisted_refs_guard()'),
    ('trigger', 'push_unlisted_refs_immutable on forge.push_unlisted_refs')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261111100200, "guarded_changes": ["forge.push_unlisted_refs.audited_change_present", "forge.push_unlisted_refs_guard", "forge.push_unlisted_refs.push_unlisted_refs_immutable"]}');
