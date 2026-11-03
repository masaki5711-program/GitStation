-- M3 R6 review: merge_settings.codeowners_restore_seen_at (20261112100100)
-- is an unaudited bookkeeping column (tables.toml unaudited_columns): the
-- audit presence trigger names it in its unaudited-columns argument, so
-- the worker's scan-order mark needs no audit row (and doctor's guard
-- check sees the trigger match the ledger). The trigger is re-created
-- with the same columns and the new argument.

DROP TRIGGER audited_change_present ON forge.merge_settings;
CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF project_id, allowed_methods, default_method,
        delete_source_default, reset_approvals_on_push, committer_approvals_excluded,
        codeowners_enforced, codeowners_suspended_at, codeowners_enforced_off_until, updated_at
    ON forge.merge_settings
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id',
        'codeowners_restore_seen_at', '');
ALTER TABLE forge.merge_settings ENABLE ALWAYS TRIGGER audited_change_present;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261112100200, "guarded_changes": ["forge.merge_settings.audited_change_present"]}');
