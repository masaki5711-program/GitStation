-- M3 R6 (ADR-0024 §4.1, §7, §8.2, §8.3, §8.6, §9, §12; plan 11 §4.1):
-- the merge conditions.
--
-- 1. merge_settings: one row per project that changed its settings (no
--    row = the defaults of forge_core::MergeSettingsValues: merge commit
--    and squash allowed, merge commit by default, the source kept, new
--    commits clear the approvals, committers' approvals excluded,
--    CODEOWNERS enforced). Written by `PUT …/merge-settings`
--    (MergeSettingsManage, `merge_settings.updated`, audit only) and by
--    the CODEOWNERS toggle (CodeownersEnforcementToggle,
--    `merge_settings.codeowners_enforcement_changed`, an event). A
--    suspension of CODEOWNERS enforcement always has an expiry at most 24
--    hours after it started (merge_settings_codeowners_suspension); the
--    evaluation counts an expired suspension as enforced by itself
--    (`codeowners_enforced OR codeowners_enforced_off_until <= now()`),
--    and the worker records the expiry once
--    (`merge_settings.codeowners_enforcement_restored`, actor system).
--
-- 2. protected_ref_merge_rules: at most one merge rule per protection rule
--    (the strictest of the matching rules applies, like push_min_role):
--    the minimum counted approvals (0..20), the required check names (at
--    most 20, `ci/` / `forge/` / `kanflow/` only — never `ext/`, ADR-0024
--    §10), resolved discussions, and the direct-push ban. The work item
--    link condition has no column until I1 (ADR-0024 §11). Managed with
--    ProtectedRefManage (`protected_ref.merge_rule_put` / `.merge_rule_removed`,
--    audit only); deleting a protection rule deletes its merge rule in the
--    same change (protected_refs.rs), the project purge deletes them
--    before the protection rules.
--
-- 3. merge_request_approvals: an approval carried to a new head when the
--    project keeps approvals on push (`reset_approvals_on_push = false`,
--    ADR-0024 §8.2) ends with revoked_reason `carried` and its copy names
--    it in carried_from. The guard keeps carried_from with the identity.
--
-- Every table is audited state on the project (resource `project`, id
-- `project_id`); forge_app writes them and deletes them in the purge.

CREATE TABLE forge.merge_settings (
    project_id                     bigint PRIMARY KEY REFERENCES forge.projects (id),
    allowed_methods                text[] NOT NULL CONSTRAINT merge_settings_allowed_methods
        CHECK (cardinality(allowed_methods) BETWEEN 1 AND 3
            AND array_position(allowed_methods, NULL) IS NULL
            AND allowed_methods <@ ARRAY['merge_commit', 'squash', 'fast_forward']::text[]),
    default_method                 text NOT NULL CONSTRAINT merge_settings_default_method
        CHECK (default_method = ANY (allowed_methods)),
    delete_source_default          boolean NOT NULL DEFAULT false,
    reset_approvals_on_push        boolean NOT NULL DEFAULT true,
    committer_approvals_excluded   boolean NOT NULL DEFAULT true,
    codeowners_enforced            boolean NOT NULL DEFAULT true,
    codeowners_suspended_at        timestamptz,
    codeowners_enforced_off_until  timestamptz,
    updated_at                     timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT merge_settings_codeowners_suspension CHECK (
        codeowners_enforced = (codeowners_enforced_off_until IS NULL)
        AND (codeowners_enforced_off_until IS NULL) = (codeowners_suspended_at IS NULL)
        AND (codeowners_enforced_off_until IS NULL
            OR (codeowners_enforced_off_until > codeowners_suspended_at
                AND codeowners_enforced_off_until
                    <= codeowners_suspended_at + interval '24 hours')))
);
-- The worker's scan for expired suspensions.
CREATE INDEX merge_settings_codeowners_expiry ON forge.merge_settings
    (codeowners_enforced_off_until) WHERE codeowners_enforced_off_until IS NOT NULL;

ALTER TABLE forge.protected_refs
    ADD CONSTRAINT protected_refs_id_project UNIQUE (id, project_id);

CREATE TABLE forge.protected_ref_merge_rules (
    protected_ref_id              bigint PRIMARY KEY,
    project_id                    bigint NOT NULL REFERENCES forge.projects (id),
    required_approvals            integer NOT NULL DEFAULT 0
        CONSTRAINT protected_ref_merge_rules_required_approvals
        CHECK (required_approvals BETWEEN 0 AND 20),
    required_checks               text[] NOT NULL DEFAULT '{}'
        CONSTRAINT protected_ref_merge_rules_required_checks
        CHECK (cardinality(required_checks) <= 20
            AND array_position(required_checks, NULL) IS NULL
            AND (cardinality(required_checks) = 0
                OR (' ' || array_to_string(required_checks, ' '))
                    ~ '^( (ci|forge|kanflow)/[a-z0-9][a-z0-9._/-]{0,99})+$')),
    require_resolved_discussions  boolean NOT NULL DEFAULT false,
    direct_push_forbidden         boolean NOT NULL DEFAULT false,
    created_at                    timestamptz NOT NULL DEFAULT now(),
    updated_at                    timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT protected_ref_merge_rules_rule
        FOREIGN KEY (protected_ref_id, project_id)
        REFERENCES forge.protected_refs (id, project_id)
);
-- The project purge.
CREATE INDEX protected_ref_merge_rules_project ON forge.protected_ref_merge_rules (project_id);

ALTER TABLE forge.merge_request_approvals
    ADD COLUMN carried_from bigint REFERENCES forge.merge_request_approvals (id);
ALTER TABLE forge.merge_request_approvals
    DROP CONSTRAINT merge_request_approvals_revoked_reason;
ALTER TABLE forge.merge_request_approvals ADD CONSTRAINT merge_request_approvals_revoked_reason
    CHECK (revoked_reason IN ('unapproved', 'new_commits', 'target_changed', 'carried'));

-- An approval's identity (now with carried_from) never changes; it ends
-- once and never comes back.
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
       OR NEW.carried_from IS DISTINCT FROM OLD.carried_from THEN
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

-- The audit presence check covers the new column too.
DROP TRIGGER audited_change_present ON forge.merge_request_approvals;
CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, merge_request_id, project_id, approver_id, head_sha,
        target_branch, created_at, revoked_at, revoked_reason, carried_from
    ON forge.merge_request_approvals
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.merge_request_approvals ENABLE ALWAYS TRIGGER audited_change_present;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF project_id, allowed_methods, default_method,
        delete_source_default, reset_approvals_on_push, committer_approvals_excluded,
        codeowners_enforced, codeowners_suspended_at, codeowners_enforced_off_until, updated_at
    ON forge.merge_settings
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.merge_settings ENABLE ALWAYS TRIGGER audited_change_present;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF protected_ref_id, project_id, required_approvals,
        required_checks, require_resolved_discussions, direct_push_forbidden, created_at,
        updated_at
    ON forge.protected_ref_merge_rules
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.protected_ref_merge_rules ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.merge_settings TO forge_app;
GRANT SELECT ON forge.merge_settings TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.protected_ref_merge_rules TO forge_app;
GRANT SELECT ON forge.protected_ref_merge_rules TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.merge_settings'),
    ('trigger', 'audited_change_present on forge.protected_ref_merge_rules')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261112100000, "guarded_changes": ["forge.merge_settings.audited_change_present", "forge.protected_ref_merge_rules.audited_change_present", "forge.merge_request_approvals_guard", "forge.merge_request_approvals.audited_change_present"]}');
