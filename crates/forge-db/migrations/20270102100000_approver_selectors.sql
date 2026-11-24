-- ADR-0038 §3 (step C): approvers by organisation selector, named people
-- and groups, mixed; delegation of approvals.
--
-- 1. approval_rule_approvers gains a third kind, `selector` (jsonb: the
--    canonical form of forge_core::approver::ApproverSelector, §3.1). The
--    one-kind CHECK becomes exactly one of user_id / group_id / selector.
--    The shape CHECK is a backstop (the application validates every key
--    and value before writing; `head_only` is refused until PFS fills the
--    heads, ADR-0038 §8.4).
--
-- 2. deploy_environments gains approvals_required (1..20: how many of the
--    approvers must approve, default 1 = as before) and allow_delegation
--    (default false: a delegate never approves a deployment unless the
--    environment says so, §3.5). deploy_environment_approvers names the
--    environment's approvers (users, groups, selectors, at most 20 / 10 /
--    5); none = any Maintainer, as before. Changing them is the action
--    DeployApproversManage (Owner and administrators, §3.6), audited on the
--    project (`deploy_environment.approvers_set`).
--
-- 3. deployment_approvals: one row per approval of a deployment (the
--    approver, whom they approved for, and the organisation snapshot of the
--    person the approval counts for — codes, levels, rank, positions and
--    the digest only, §3.1, §6). A deployment moves to `queued` once the
--    distinct people it counts for reach the environment's
--    approvals_required. Derived (its writes ride on `deployment.approved`
--    in the same transaction).
--
-- 4. approval_delegations: at most one delegation per person (the
--    delegator; a new one replaces it): the delegate approves for them from
--    starts_at until ends_at, at most 30 days, never themself. Set and
--    removed by the delegator in a web session, audited on the delegator
--    (`approval_delegation.set` / `.removed`).
--
-- 5. merge_request_approvals gains on_behalf_of (the delegator, §3.5) and
--    approver_org (the organisation snapshot of the person the approval
--    counts for, recorded at approval time, §3.1). Both are part of the
--    approval's identity (the guard), and a carried approval copies them.

-- 1 ------------------------------------------------------------------------
ALTER TABLE forge.approval_rule_approvers ADD COLUMN selector jsonb;
ALTER TABLE forge.approval_rule_approvers DROP CONSTRAINT approval_rule_approvers_one_kind;
ALTER TABLE forge.approval_rule_approvers ADD CONSTRAINT approval_rule_approvers_one_kind
    CHECK (num_nonnulls(user_id, group_id, selector) = 1);
ALTER TABLE forge.approval_rule_approvers ADD CONSTRAINT approval_rule_approvers_selector_shape
    CHECK (selector IS NULL OR (
        jsonb_typeof(selector) = 'object'
        AND selector ->> 'relation' IN ('requester_section', 'requester_department', 'unit')
        AND coalesce(selector ->> 'escalate', 'none') IN ('none', 'parent')
        AND coalesce(selector ->> 'head_only', 'false') = 'false'
        AND octet_length(selector::text) <= 1024));

DROP TRIGGER audited_change_present ON forge.approval_rule_approvers;
CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, rule_id, project_id, user_id, group_id, selector
    ON forge.approval_rule_approvers
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.approval_rule_approvers ENABLE ALWAYS TRIGGER audited_change_present;

-- 2 ------------------------------------------------------------------------
ALTER TABLE forge.deploy_environments
    ADD COLUMN approvals_required integer NOT NULL DEFAULT 1
        CONSTRAINT deploy_environments_approvals_required CHECK (approvals_required BETWEEN 1 AND 20),
    ADD COLUMN allow_delegation boolean NOT NULL DEFAULT false;
ALTER TABLE forge.deploy_environments
    ADD CONSTRAINT deploy_environments_id_project UNIQUE (id, project_id);

DROP TRIGGER audited_change_present ON forge.deploy_environments;
CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, project_id, tier, requires_approval,
        approval_ttl_minutes, auto_rollback, auto_deploy_ref, freeze_windows, created_by,
        updated_by, created_at, approvals_required, allow_delegation
    ON forge.deploy_environments
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', 'updated_at', '');
ALTER TABLE forge.deploy_environments ENABLE ALWAYS TRIGGER audited_change_present;

CREATE TABLE forge.deploy_environment_approvers (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    environment_id  bigint NOT NULL,
    project_id      bigint NOT NULL REFERENCES forge.projects (id),
    user_id         bigint REFERENCES forge.users (id),
    group_id        bigint REFERENCES forge.groups (id),
    selector        jsonb,
    CONSTRAINT deploy_environment_approvers_one_kind
        CHECK (num_nonnulls(user_id, group_id, selector) = 1),
    CONSTRAINT deploy_environment_approvers_selector_shape
        CHECK (selector IS NULL OR (
            jsonb_typeof(selector) = 'object'
            AND selector ->> 'relation' IN ('requester_section', 'requester_department', 'unit')
            AND coalesce(selector ->> 'escalate', 'none') IN ('none', 'parent')
            AND coalesce(selector ->> 'head_only', 'false') = 'false'
            AND octet_length(selector::text) <= 1024)),
    CONSTRAINT deploy_environment_approvers_environment
        FOREIGN KEY (environment_id, project_id)
        REFERENCES forge.deploy_environments (id, project_id),
    CONSTRAINT deploy_environment_approvers_user UNIQUE (environment_id, user_id),
    CONSTRAINT deploy_environment_approvers_group UNIQUE (environment_id, group_id)
);
CREATE INDEX deploy_environment_approvers_project
    ON forge.deploy_environment_approvers (project_id);
CREATE INDEX deploy_environment_approvers_by_user ON forge.deploy_environment_approvers (user_id);
CREATE INDEX deploy_environment_approvers_by_group ON forge.deploy_environment_approvers (group_id);

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, environment_id, project_id, user_id, group_id,
        selector
    ON forge.deploy_environment_approvers
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.deploy_environment_approvers ENABLE ALWAYS TRIGGER audited_change_present;

-- 3 ------------------------------------------------------------------------
CREATE TABLE forge.deployment_approvals (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    deployment_id  bigint NOT NULL REFERENCES forge.deployments (id),
    project_id     bigint NOT NULL REFERENCES forge.projects (id),
    approver_id    bigint NOT NULL REFERENCES forge.users (id),
    on_behalf_of   bigint REFERENCES forge.users (id),
    approver_org   jsonb CONSTRAINT deployment_approvals_approver_org
        CHECK (jsonb_typeof(approver_org) = 'object' AND octet_length(approver_org::text) <= 4096),
    created_at     timestamptz NOT NULL DEFAULT now(),
    -- The role the approval's gate asked for (named: reporter; group,
    -- selector or no list: maintainer), which the delegator must still hold
    -- for the approval to count (review of step C). Only with on_behalf_of.
    delegator_required_role text CONSTRAINT deployment_approvals_delegator_role
        CHECK (delegator_required_role IN ('guest', 'reporter', 'developer', 'maintainer', 'owner')),
    CONSTRAINT deployment_approvals_delegator_role_pair
        CHECK ((on_behalf_of IS NULL) = (delegator_required_role IS NULL)),
    CONSTRAINT deployment_approvals_not_self CHECK (on_behalf_of IS DISTINCT FROM approver_id),
    CONSTRAINT deployment_approvals_one_per_approver UNIQUE (deployment_id, approver_id)
);
-- One approval per person it counts for (a delegate and the delegator
-- never both count).
CREATE UNIQUE INDEX deployment_approvals_one_per_person
    ON forge.deployment_approvals (deployment_id, coalesce(on_behalf_of, approver_id));
CREATE INDEX deployment_approvals_project ON forge.deployment_approvals (project_id);
CREATE INDEX deployment_approvals_approver ON forge.deployment_approvals (approver_id);
CREATE INDEX deployment_approvals_on_behalf_of ON forge.deployment_approvals (on_behalf_of);

-- 4 ------------------------------------------------------------------------
CREATE TABLE forge.approval_delegations (
    delegator_id  bigint PRIMARY KEY REFERENCES forge.users (id),
    delegate_id   bigint NOT NULL REFERENCES forge.users (id),
    starts_at     timestamptz NOT NULL,
    ends_at       timestamptz NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT approval_delegations_not_self CHECK (delegate_id <> delegator_id),
    -- 720 hours, not '30 days': a day interval depends on the session's
    -- TimeZone across a DST change; hours do not (UTC-pinned, the same
    -- bound the application checks).
    CONSTRAINT approval_delegations_period CHECK (
        ends_at > starts_at AND ends_at <= starts_at + interval '720 hours')
);
CREATE INDEX approval_delegations_delegate ON forge.approval_delegations (delegate_id);

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF delegator_id, delegate_id, starts_at, ends_at, created_at
    ON forge.approval_delegations
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('user', 'delegator_id', '', '');
ALTER TABLE forge.approval_delegations ENABLE ALWAYS TRIGGER audited_change_present;

-- 5 ------------------------------------------------------------------------
ALTER TABLE forge.merge_request_approvals
    ADD COLUMN on_behalf_of bigint REFERENCES forge.users (id),
    ADD COLUMN approver_org jsonb CONSTRAINT merge_request_approvals_approver_org
        CHECK (jsonb_typeof(approver_org) = 'object' AND octet_length(approver_org::text) <= 4096);
-- NOT VALID, then VALIDATE: the table is large on a long-running instance;
-- VALIDATE takes only SHARE UPDATE EXCLUSIVE (writes go on meanwhile).
ALTER TABLE forge.merge_request_approvals ADD CONSTRAINT merge_request_approvals_not_self
    CHECK (on_behalf_of IS DISTINCT FROM approver_id) NOT VALID;
ALTER TABLE forge.merge_request_approvals VALIDATE CONSTRAINT merge_request_approvals_not_self;
-- One live approval per person it counts for (review of step C): a person
-- and their delegate never both hold one on a merge request.
CREATE UNIQUE INDEX merge_request_approvals_one_live_per_person
    ON forge.merge_request_approvals (merge_request_id, coalesce(on_behalf_of, approver_id))
    WHERE revoked_at IS NULL;
-- An approval given on someone's behalf ends when they remove or replace
-- the delegation (`delegation_ended`).
ALTER TABLE forge.merge_request_approvals DROP CONSTRAINT merge_request_approvals_revoked_reason;
ALTER TABLE forge.merge_request_approvals ADD CONSTRAINT merge_request_approvals_revoked_reason
    CHECK (revoked_reason IN ('unapproved', 'new_commits', 'target_changed', 'carried',
        'delegation_ended'));
CREATE INDEX merge_request_approvals_on_behalf_of
    ON forge.merge_request_approvals (on_behalf_of) WHERE on_behalf_of IS NOT NULL;

-- An approval's identity (now with on_behalf_of and approver_org) never
-- changes; it ends once and never comes back.
CREATE OR REPLACE FUNCTION forge.merge_request_approvals_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NEW.merge_request_id IS DISTINCT FROM OLD.merge_request_id
       OR NEW.project_id IS DISTINCT FROM OLD.project_id
       OR NEW.approver_id IS DISTINCT FROM OLD.approver_id
       OR NEW.head_sha IS DISTINCT FROM OLD.head_sha
       OR NEW.target_branch IS DISTINCT FROM OLD.target_branch
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
       OR NEW.carried_from IS DISTINCT FROM OLD.carried_from
       OR NEW.on_behalf_of IS DISTINCT FROM OLD.on_behalf_of
       OR NEW.approver_org IS DISTINCT FROM OLD.approver_org THEN
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

DROP TRIGGER audited_change_present ON forge.merge_request_approvals;
CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, merge_request_id, project_id, approver_id, head_sha,
        target_branch, created_at, revoked_at, revoked_reason, carried_from, credential_origin,
        on_behalf_of, approver_org
    ON forge.merge_request_approvals
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.merge_request_approvals ENABLE ALWAYS TRIGGER audited_change_present;

-- Grants ------------------------------------------------------------------
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.deploy_environment_approvers TO forge_app;
GRANT SELECT ON forge.deploy_environment_approvers TO forge_ro;
-- Rows are written once and never changed; DELETE for the approvers'
-- replacement (pending approvals end) and the project purge.
GRANT SELECT, INSERT, DELETE ON forge.deployment_approvals TO forge_app;
GRANT SELECT ON forge.deployment_approvals TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.approval_delegations TO forge_app;
GRANT SELECT ON forge.approval_delegations TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.deploy_environment_approvers'),
    ('trigger', 'audited_change_present on forge.approval_delegations')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20270102100000, "guarded_changes": ["forge.approval_rule_approvers.audited_change_present", "forge.deploy_environments.audited_change_present", "forge.deploy_environment_approvers.audited_change_present", "forge.approval_delegations.audited_change_present", "forge.merge_request_approvals_guard", "forge.merge_request_approvals.audited_change_present"]}');
