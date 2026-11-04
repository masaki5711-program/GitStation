-- M3 I1d (ADR-0024 §7 / §11 and its I1d note, ADR-0016 Amendment I1d):
-- the work item link merge condition. R6 left it without a column ("I1
-- adds the column and the condition together"); I1b brought the links
-- (forge.merge_request_work_items), so a protected branch's merge rule can
-- now require one: a merge request merges into the branch only while it
-- has an open link to a work item of its project's current binding
-- (`work_item_link_missing` otherwise).
--
-- The table is audited state on the project: the audited_change_present
-- trigger is re-created with the new column in UPDATE OF (the ledger lists
-- no unaudited column), so a change of it alone still needs the
-- protected_ref.merge_rule_put audit row.

ALTER TABLE forge.protected_ref_merge_rules
    ADD COLUMN require_work_item_link boolean NOT NULL DEFAULT false;

DROP TRIGGER audited_change_present ON forge.protected_ref_merge_rules;
CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF protected_ref_id, project_id, required_approvals,
        required_checks, require_resolved_discussions, direct_push_forbidden,
        require_work_item_link, created_at, updated_at
    ON forge.protected_ref_merge_rules
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.protected_ref_merge_rules ENABLE ALWAYS TRIGGER audited_change_present;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261119100000, "guarded_changes": ["forge.protected_ref_merge_rules.audited_change_present"]}');
