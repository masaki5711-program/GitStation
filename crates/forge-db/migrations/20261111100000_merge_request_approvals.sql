-- M3 R5 (ADR-0024 §8, §9, §12; plan 11 §4.1 R5, §11 decision 9): approvals
-- of merge requests and the projects' approval rules.
--
-- 1. The token scope `merge_approve` (ADR-0024 §8.5, ADR-0017 §4.0): an
--    approval needs it, and `api`, `admin` and `merge_execute` do not satisfy
--    it. tokens_scopes_known accepted six values (20261110100000); it is
--    replaced by the same CHECK with the seventh (the variant and its CHECK
--    land in the same MR). Appended instead of editing (append-only,
--    ADR-0014). The tokens table is small, so the CHECK is replaced at once
--    (a large table would take NOT VALID + VALIDATE, ADR-0024 R4).
--
-- 2. merge_request_approvals: one row per approval. An approval is bound to
--    the head and the target the approver saw (ADR-0024 §8.1): head_sha and
--    target_branch are copied from the merge request row under its lock,
--    and evaluate_merge() counts only rows whose pair equals the merge
--    request's current pair (stale rows can never count, by construction).
--    An approval ends once (revoked_at / revoked_reason, set together, never
--    cleared): `unapproved` (the approver withdrew it), `new_commits` (a new
--    head was recorded, the default of plan 11 Q4), `target_changed` (the
--    merge request was retargeted). At most one live (unrevoked) approval
--    per approver and merge request (merge_request_approvals_one_live).
--    Audited on the project: every write commits with a
--    `merge_request.approved` / `merge_request.unapproved` audit row (the
--    version job's revocation rides on its `merge_request.pushed` row with
--    one `affected` row per revoked approval).
--
-- 3. approval_rules and approval_rule_approvers (ADR-0024 §8.6): a rule
--    names how many approvals a merge request needs (1..20), optionally for
--    target branches matching target_pattern (a RefPattern: NULL = every
--    target), and who may give them (users and groups; none = any eligible
--    Developer and above). Rule changes are audited on the project
--    (`approval_rule.put` / `approval_rule.removed`, audit only) and never
--    touch existing approvals: they change evaluate_merge()'s input only.
--
-- forge_app writes them (the approval routes, the version job, the edit
-- command, the rule routes) and deletes them in the project purge
-- (projects/purge.rs, approvals and approvers before the rules, before the
-- merge requests).

ALTER TABLE forge.tokens DROP CONSTRAINT tokens_scopes_known;
ALTER TABLE forge.tokens ADD CONSTRAINT tokens_scopes_known CHECK (
    cardinality(scopes) >= 1
    AND scopes <@ ARRAY['read_repository', 'write_repository', 'read_api', 'api', 'admin',
        'merge_execute', 'merge_approve']::text[]);

CREATE TABLE forge.merge_request_approvals (
    id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    merge_request_id  bigint NOT NULL,
    project_id        bigint NOT NULL REFERENCES forge.projects (id),
    approver_id       bigint NOT NULL REFERENCES forge.users (id),
    head_sha          text NOT NULL CONSTRAINT merge_request_approvals_head_sha
        CHECK (head_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    target_branch     text COLLATE "C" NOT NULL CONSTRAINT merge_request_approvals_target_branch
        CHECK (target_branch ~ '^refs/heads/.' AND octet_length(target_branch) <= 1024),
    created_at        timestamptz NOT NULL DEFAULT now(),
    revoked_at        timestamptz,
    revoked_reason    text CONSTRAINT merge_request_approvals_revoked_reason
        CHECK (revoked_reason IN ('unapproved', 'new_commits', 'target_changed')),
    CONSTRAINT merge_request_approvals_revoked_fields
        CHECK ((revoked_at IS NULL) = (revoked_reason IS NULL)),
    CONSTRAINT merge_request_approvals_merge_request
        FOREIGN KEY (merge_request_id, project_id)
        REFERENCES forge.merge_requests (id, project_id)
);
-- One live approval per approver and merge request.
CREATE UNIQUE INDEX merge_request_approvals_one_live
    ON forge.merge_request_approvals (merge_request_id, approver_id) WHERE revoked_at IS NULL;
-- The approvals of a merge request, oldest first.
CREATE INDEX merge_request_approvals_merge_request
    ON forge.merge_request_approvals (merge_request_id, id);
-- The project purge.
CREATE INDEX merge_request_approvals_project ON forge.merge_request_approvals (project_id);

CREATE TABLE forge.approval_rules (
    id                  bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    project_id          bigint NOT NULL REFERENCES forge.projects (id),
    name                text COLLATE "C" NOT NULL CONSTRAINT approval_rules_name
        CHECK (name ~ '^[a-z0-9][a-z0-9_.-]{0,63}$'),
    target_pattern      text COLLATE "C" CONSTRAINT approval_rules_target_pattern
        CHECK (octet_length(target_pattern) BETWEEN 1 AND 1024),
    required_approvals  integer NOT NULL CONSTRAINT approval_rules_required
        CHECK (required_approvals BETWEEN 1 AND 20),
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT approval_rules_project_name UNIQUE (project_id, name),
    CONSTRAINT approval_rules_id_project UNIQUE (id, project_id)
);

CREATE TABLE forge.approval_rule_approvers (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    rule_id     bigint NOT NULL,
    project_id  bigint NOT NULL,
    user_id     bigint REFERENCES forge.users (id),
    group_id    bigint REFERENCES forge.groups (id),
    CONSTRAINT approval_rule_approvers_one_kind
        CHECK ((user_id IS NULL) <> (group_id IS NULL)),
    CONSTRAINT approval_rule_approvers_rule
        FOREIGN KEY (rule_id, project_id) REFERENCES forge.approval_rules (id, project_id),
    CONSTRAINT approval_rule_approvers_user UNIQUE (rule_id, user_id),
    CONSTRAINT approval_rule_approvers_group UNIQUE (rule_id, group_id)
);
CREATE INDEX approval_rule_approvers_project ON forge.approval_rule_approvers (project_id);

-- An approval's identity never changes; it ends once (revoked_at and
-- revoked_reason set together from NULL) and never comes back.
CREATE FUNCTION forge.merge_request_approvals_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NEW.merge_request_id IS DISTINCT FROM OLD.merge_request_id
       OR NEW.project_id IS DISTINCT FROM OLD.project_id
       OR NEW.approver_id IS DISTINCT FROM OLD.approver_id
       OR NEW.head_sha IS DISTINCT FROM OLD.head_sha
       OR NEW.target_branch IS DISTINCT FROM OLD.target_branch
       OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the identity of an approval on forge.merge_request_approvals never changes';
    END IF;
    IF OLD.revoked_at IS NOT NULL AND NEW IS DISTINCT FROM OLD THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'an ended approval on forge.merge_request_approvals never changes';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.merge_request_approvals_guard() FROM PUBLIC;
CREATE TRIGGER merge_request_approvals_end_once BEFORE UPDATE ON forge.merge_request_approvals
    FOR EACH ROW EXECUTE FUNCTION forge.merge_request_approvals_guard();
ALTER TABLE forge.merge_request_approvals ENABLE ALWAYS TRIGGER merge_request_approvals_end_once;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, merge_request_id, project_id, approver_id, head_sha,
        target_branch, created_at, revoked_at, revoked_reason
    ON forge.merge_request_approvals
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.merge_request_approvals ENABLE ALWAYS TRIGGER audited_change_present;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, project_id, name, target_pattern, required_approvals,
        created_at, updated_at
    ON forge.approval_rules
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.approval_rules ENABLE ALWAYS TRIGGER audited_change_present;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, rule_id, project_id, user_id, group_id
    ON forge.approval_rule_approvers
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.approval_rule_approvers ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.merge_request_approvals TO forge_app;
GRANT SELECT ON forge.merge_request_approvals TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.approval_rules TO forge_app;
GRANT SELECT ON forge.approval_rules TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.approval_rule_approvers TO forge_app;
GRANT SELECT ON forge.approval_rule_approvers TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.merge_request_approvals'),
    ('trigger', 'audited_change_present on forge.approval_rules'),
    ('trigger', 'audited_change_present on forge.approval_rule_approvers'),
    ('function', 'forge.merge_request_approvals_guard()'),
    ('trigger', 'merge_request_approvals_end_once on forge.merge_request_approvals')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261111100000, "guarded_changes": ["forge.merge_request_approvals.audited_change_present", "forge.approval_rules.audited_change_present", "forge.approval_rule_approvers.audited_change_present", "forge.merge_request_approvals_guard", "forge.merge_request_approvals.merge_request_approvals_end_once"]}');
