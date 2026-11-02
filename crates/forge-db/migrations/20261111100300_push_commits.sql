-- M3 R5 round 4 (ADR-0024 §8.3, "R5 の再レビュー（4 回目）"): the committer rule
-- is recorded at receive time instead of being reconstructed from the push
-- audit rows (which list at most 100 refs and 20 commits a ref, so every cut
-- needed a reachability walk with its own caps — each one a cheap way to block
-- approvals).
--
-- push_commits: one row per (repository, commit, pusher) for every commit a
-- push introduced — made reachable when no ref reached it before the push
-- (forge-git push_introduced). A ref update that brings no new commit (a tag
-- or a branch on an existing commit, a delete) writes nothing. Written by the
-- push record (repositories::record) in the same transaction as its audit
-- row, `ON CONFLICT DO NOTHING` (the first push of a commit by a pusher stays);
-- a merge's own commit by the merge record (the requester). request_id ties a
-- row to its `repository.pushed` audit row (the audit row's id is assigned at
-- its commit). Audited on the project (the push record is the change), never
-- updated (guard push_commits_immutable), deleted only by the project purge.
-- The committers of a merge request are the pushers of its range's commits;
-- a range commit with no row (history before this table, an update-ref import,
-- a push past the per-push cap) makes them unknown (fail closed).
--
-- The reachability design's leftovers: push_unlisted_refs is no longer written
-- or read (kept: append-only history; a later migration may drop it), and the
-- audit log index audit_log_pushes (20261111100100) had no other reader.

CREATE TABLE forge.push_commits (
    repository_id    bigint NOT NULL,
    commit_sha       text NOT NULL CONSTRAINT push_commits_commit_sha
        CHECK (commit_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    pusher_id        bigint NOT NULL REFERENCES forge.users (id),
    project_id       bigint NOT NULL REFERENCES forge.projects (id),
    request_id       text NOT NULL CONSTRAINT push_commits_request_id
        CHECK (char_length(request_id) BETWEEN 1 AND 128),
    first_pushed_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT push_commits_pkey PRIMARY KEY (repository_id, commit_sha, pusher_id)
);
-- The project purge.
CREATE INDEX push_commits_project ON forge.push_commits (project_id);

CREATE FUNCTION forge.push_commits_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    RAISE EXCEPTION USING ERRCODE = 'XF001',
        MESSAGE = 'a row of forge.push_commits is never changed';
END
$$;
REVOKE ALL ON FUNCTION forge.push_commits_guard() FROM PUBLIC;
CREATE TRIGGER push_commits_immutable BEFORE UPDATE ON forge.push_commits
    FOR EACH ROW EXECUTE FUNCTION forge.push_commits_guard();
ALTER TABLE forge.push_commits ENABLE ALWAYS TRIGGER push_commits_immutable;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF repository_id, commit_sha, pusher_id, project_id,
        request_id, first_pushed_at
    ON forge.push_commits
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.push_commits ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.push_commits TO forge_app;
GRANT SELECT ON forge.push_commits TO forge_ro;

DROP INDEX forge.audit_log_pushes;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.push_commits'),
    ('function', 'forge.push_commits_guard()'),
    ('trigger', 'push_commits_immutable on forge.push_commits')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261111100300, "guarded_changes": ["forge.audit_log", "forge.push_commits.audited_change_present", "forge.push_commits_guard", "forge.push_commits.push_commits_immutable"]}');
