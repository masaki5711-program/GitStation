-- M3 R1a (ADR-0024 §1, §6; plan 11 §4.1, §11): merge requests.
--
-- merge_requests: one row per merge request of a project. `iid` is the
-- project's own number (unique per project), drawn from
-- merge_request_counters under the project row lock (never max(iid) + 1:
-- the purge deletes rows, and a counter never goes back). Both branches are
-- `refs/heads/*` names and differ (the application refuses them first,
-- ADR-0024 §1, and authorize() denies them; these CHECKs are the backstop).
-- At most one open merge request per (project, source, target): the partial
-- unique index merge_requests_one_open backs FG-REVIEW-0001. `revision`
-- counts the row's changes (1 at creation, +1 per change): the events'
-- dedup keys are `<prefix>:<id>:<revision>` (server ids only). `head_sha`
-- stays NULL until R1b adds versions on push; merge_request_versions is
-- created by R1b, with its first writer.
--
-- The state machine (ADR-0024 §6) is held by the application and by the
-- guard merge_requests_state_guard: `merged` is final, and `closed` never
-- becomes `merged` directly (reopen first). closed_at / closed_by are set
-- exactly while the merge request is closed.
--
-- Both tables are audited state on the project (resource `project`, id
-- `project_id`, like protected_refs): every change commits with a
-- `merge_request.*` audit row on the project, and the purge's
-- `project.purged` row covers their deletion. forge_app may DELETE for the
-- purge (projects/purge.rs), as for protected_refs.

CREATE TABLE forge.merge_requests (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    project_id     bigint NOT NULL REFERENCES forge.projects (id),
    iid            bigint NOT NULL CONSTRAINT merge_requests_iid_positive CHECK (iid > 0),
    title          text NOT NULL CONSTRAINT merge_requests_title_length
        CHECK (char_length(title) BETWEEN 1 AND 255),
    description    text NOT NULL DEFAULT '' CONSTRAINT merge_requests_description_length
        CHECK (octet_length(description) <= 65536),
    author_id      bigint NOT NULL REFERENCES forge.users (id),
    source_branch  text COLLATE "C" NOT NULL
        CONSTRAINT merge_requests_source_branch CHECK (source_branch ~ '^refs/heads/.'
            AND octet_length(source_branch) <= 1024),
    target_branch  text COLLATE "C" NOT NULL
        CONSTRAINT merge_requests_target_branch CHECK (target_branch ~ '^refs/heads/.'
            AND octet_length(target_branch) <= 1024),
    state          text NOT NULL DEFAULT 'open' CONSTRAINT merge_requests_state
        CHECK (state IN ('open', 'merged', 'closed')),
    draft          boolean NOT NULL DEFAULT false,
    head_sha       text CONSTRAINT merge_requests_head_sha
        CHECK (head_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    revision       bigint NOT NULL DEFAULT 1 CONSTRAINT merge_requests_revision_positive
        CHECK (revision > 0),
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    closed_at      timestamptz,
    closed_by      bigint REFERENCES forge.users (id),
    CONSTRAINT merge_requests_branches_differ CHECK (source_branch <> target_branch),
    CONSTRAINT merge_requests_closed_fields CHECK (
        (state = 'closed') = (closed_at IS NOT NULL)
        AND (closed_at IS NULL) = (closed_by IS NULL)),
    CONSTRAINT merge_requests_project_iid UNIQUE (project_id, iid)
);
CREATE UNIQUE INDEX merge_requests_one_open
    ON forge.merge_requests (project_id, source_branch, target_branch) WHERE state = 'open';
-- The list: a project's merge requests by state, newest first.
CREATE INDEX merge_requests_project_state ON forge.merge_requests (project_id, state, iid);

CREATE FUNCTION forge.merge_requests_state_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NEW.state IS DISTINCT FROM OLD.state
       AND (OLD.state = 'merged' OR (OLD.state = 'closed' AND NEW.state = 'merged')) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a merge request on forge.merge_requests never leaves merged, and is reopened before it is merged';
    END IF;
    IF NEW.project_id IS DISTINCT FROM OLD.project_id OR NEW.iid IS DISTINCT FROM OLD.iid
       OR NEW.author_id IS DISTINCT FROM OLD.author_id
       OR NEW.source_branch IS DISTINCT FROM OLD.source_branch THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the project, number, author and source branch of a merge request on forge.merge_requests never change';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.merge_requests_state_guard() FROM PUBLIC;
CREATE TRIGGER merge_requests_state_forward BEFORE UPDATE ON forge.merge_requests
    FOR EACH ROW EXECUTE FUNCTION forge.merge_requests_state_guard();
ALTER TABLE forge.merge_requests ENABLE ALWAYS TRIGGER merge_requests_state_forward;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, project_id, iid, title, description, author_id,
        source_branch, target_branch, state, draft, head_sha, revision, created_at, closed_at,
        closed_by
    ON forge.merge_requests
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', 'updated_at', '');
ALTER TABLE forge.merge_requests ENABLE ALWAYS TRIGGER audited_change_present;

-- The last iid drawn per project (the row appears with the first merge
-- request; `INSERT … ON CONFLICT DO UPDATE` under the project row lock).
CREATE TABLE forge.merge_request_counters (
    project_id  bigint PRIMARY KEY REFERENCES forge.projects (id),
    last_iid    bigint NOT NULL CONSTRAINT merge_request_counters_last_iid_positive
        CHECK (last_iid > 0)
);

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF project_id, last_iid ON forge.merge_request_counters
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.merge_request_counters ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.merge_requests TO forge_app;
GRANT SELECT ON forge.merge_requests TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.merge_request_counters TO forge_app;
GRANT SELECT ON forge.merge_request_counters TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.merge_requests'),
    ('trigger', 'audited_change_present on forge.merge_request_counters'),
    ('function', 'forge.merge_requests_state_guard()'),
    ('trigger', 'merge_requests_state_forward on forge.merge_requests')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261106100000, "guarded_changes": ["forge.merge_requests.audited_change_present", "forge.merge_request_counters.audited_change_present", "forge.merge_requests_state_guard", "forge.merge_requests.merge_requests_state_forward"]}');
