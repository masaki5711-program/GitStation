-- M3 R3 (ADR-0024 §6, §12; plan 11 §4.1): merge request discussions.
--
-- merge_request_discussions: one row per thread of a merge request.
-- `general` threads are on the merge request; `inline` threads sit on one
-- line of one side of one version's diff (merge_base..head, ADR-0024 R2):
-- version_no (a version of the same merge request: the composite foreign
-- key), path, side (`old` = the version's merge base, `new` = its head)
-- and line. forge-review checks the position against that diff before the
-- write (FG-REVIEW-0004); these CHECKs are the shape's backstop. Whether
-- a later version changed the line (`outdated`) is computed when the
-- threads are read, from the versions (never stored: it changes with every
-- push and needs git). started_by is the author of the first note.
-- resolved / resolved_by / resolved_at hold the thread's state; every
-- resolve or unresolve adds one to `revision` (1 at creation), the dedup
-- key of its `merge_request.commented` event
-- (`merge_request_thread_resolved|unresolved:<id>:<revision>`).
--
-- merge_request_notes: the comments of a thread, oldest first. The body is
-- plain text / Markdown stored as written (never rendered by the server),
-- 1 character to 64 KiB. Notes are never changed (no editing in R3: the
-- guard refuses every UPDATE); their event dedups on the note id
-- (`merge_request_note_added:<id>`).
--
-- Both tables are audited state on the project (resource `project`, id
-- `project_id`, like merge_requests): every write commits with a
-- `merge_request.commented` audit row on the project, and the project
-- purge's `project.purged` row covers their deletion (forge_app may DELETE
-- for the purge, projects/purge.rs, notes before discussions before
-- versions).
--
-- The guard merge_request_discussions_guard keeps a thread's identity and
-- position fixed and makes every change add one to its revision; the
-- guard merge_request_notes_guard refuses every update of a note.

CREATE TABLE forge.merge_request_discussions (
    id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    merge_request_id  bigint NOT NULL REFERENCES forge.merge_requests (id),
    project_id        bigint NOT NULL REFERENCES forge.projects (id),
    kind              text NOT NULL CONSTRAINT merge_request_discussions_kind
        CHECK (kind IN ('general', 'inline')),
    version_no        bigint CONSTRAINT merge_request_discussions_version_positive
        CHECK (version_no > 0),
    path              text COLLATE "C" CONSTRAINT merge_request_discussions_path
        CHECK (octet_length(path) BETWEEN 1 AND 4096),
    side              text CONSTRAINT merge_request_discussions_side
        CHECK (side IN ('old', 'new')),
    line              bigint CONSTRAINT merge_request_discussions_line_positive
        CHECK (line > 0),
    started_by        bigint NOT NULL REFERENCES forge.users (id),
    resolved          boolean NOT NULL DEFAULT false,
    resolved_by       bigint REFERENCES forge.users (id),
    resolved_at       timestamptz,
    revision          bigint NOT NULL DEFAULT 1 CONSTRAINT merge_request_discussions_revision_positive
        CHECK (revision > 0),
    created_at        timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT merge_request_discussions_position CHECK (
        (kind = 'general'
            AND version_no IS NULL AND path IS NULL AND side IS NULL AND line IS NULL)
        OR (kind = 'inline'
            AND version_no IS NOT NULL AND path IS NOT NULL AND side IS NOT NULL
            AND line IS NOT NULL)),
    CONSTRAINT merge_request_discussions_resolved_fields CHECK (
        resolved = (resolved_at IS NOT NULL) AND (resolved_at IS NULL) = (resolved_by IS NULL)),
    CONSTRAINT merge_request_discussions_version
        FOREIGN KEY (merge_request_id, version_no)
        REFERENCES forge.merge_request_versions (merge_request_id, version_no)
);
-- The threads of a merge request, oldest first.
CREATE INDEX merge_request_discussions_merge_request
    ON forge.merge_request_discussions (merge_request_id, id);
-- The project purge deletes a project's threads.
CREATE INDEX merge_request_discussions_project ON forge.merge_request_discussions (project_id);

CREATE TABLE forge.merge_request_notes (
    id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    discussion_id     bigint NOT NULL REFERENCES forge.merge_request_discussions (id),
    merge_request_id  bigint NOT NULL REFERENCES forge.merge_requests (id),
    project_id        bigint NOT NULL REFERENCES forge.projects (id),
    author_id         bigint NOT NULL REFERENCES forge.users (id),
    body              text NOT NULL CONSTRAINT merge_request_notes_body_length
        CHECK (char_length(body) >= 1 AND octet_length(body) <= 65536),
    created_at        timestamptz NOT NULL DEFAULT now()
);
-- The notes of a merge request (the list reads them all at once) and of a
-- thread, oldest first.
CREATE INDEX merge_request_notes_merge_request
    ON forge.merge_request_notes (merge_request_id, discussion_id, id);
CREATE INDEX merge_request_notes_discussion ON forge.merge_request_notes (discussion_id);
CREATE INDEX merge_request_notes_project ON forge.merge_request_notes (project_id);

CREATE FUNCTION forge.merge_request_discussions_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NEW.merge_request_id IS DISTINCT FROM OLD.merge_request_id
       OR NEW.project_id IS DISTINCT FROM OLD.project_id
       OR NEW.kind IS DISTINCT FROM OLD.kind
       OR NEW.version_no IS DISTINCT FROM OLD.version_no
       OR NEW.path IS DISTINCT FROM OLD.path
       OR NEW.side IS DISTINCT FROM OLD.side
       OR NEW.line IS DISTINCT FROM OLD.line
       OR NEW.started_by IS DISTINCT FROM OLD.started_by
       OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the identity and position of a discussion on forge.merge_request_discussions never change';
    END IF;
    IF NEW IS DISTINCT FROM OLD AND NEW.revision IS DISTINCT FROM OLD.revision + 1 THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'every change of a discussion on forge.merge_request_discussions adds one to its revision';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.merge_request_discussions_guard() FROM PUBLIC;
CREATE TRIGGER merge_request_discussions_identity BEFORE UPDATE ON forge.merge_request_discussions
    FOR EACH ROW EXECUTE FUNCTION forge.merge_request_discussions_guard();
ALTER TABLE forge.merge_request_discussions ENABLE ALWAYS TRIGGER merge_request_discussions_identity;

CREATE FUNCTION forge.merge_request_notes_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    RAISE EXCEPTION USING ERRCODE = 'XF001',
        MESSAGE = 'a note on forge.merge_request_notes is never changed';
END
$$;
REVOKE ALL ON FUNCTION forge.merge_request_notes_guard() FROM PUBLIC;
CREATE TRIGGER merge_request_notes_immutable BEFORE UPDATE ON forge.merge_request_notes
    FOR EACH ROW EXECUTE FUNCTION forge.merge_request_notes_guard();
ALTER TABLE forge.merge_request_notes ENABLE ALWAYS TRIGGER merge_request_notes_immutable;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, merge_request_id, project_id, kind, version_no, path,
        side, line, started_by, resolved, resolved_by, resolved_at, revision, created_at
    ON forge.merge_request_discussions
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.merge_request_discussions ENABLE ALWAYS TRIGGER audited_change_present;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, discussion_id, merge_request_id, project_id,
        author_id, body, created_at
    ON forge.merge_request_notes
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.merge_request_notes ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.merge_request_discussions TO forge_app;
GRANT SELECT ON forge.merge_request_discussions TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.merge_request_notes TO forge_app;
GRANT SELECT ON forge.merge_request_notes TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.merge_request_discussions'),
    ('trigger', 'audited_change_present on forge.merge_request_notes'),
    ('function', 'forge.merge_request_discussions_guard()'),
    ('trigger', 'merge_request_discussions_identity on forge.merge_request_discussions'),
    ('function', 'forge.merge_request_notes_guard()'),
    ('trigger', 'merge_request_notes_immutable on forge.merge_request_notes')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261109100000, "guarded_changes": ["forge.merge_request_discussions.audited_change_present", "forge.merge_request_notes.audited_change_present", "forge.merge_request_discussions_guard", "forge.merge_request_discussions.merge_request_discussions_identity", "forge.merge_request_notes_guard", "forge.merge_request_notes.merge_request_notes_immutable"]}');
