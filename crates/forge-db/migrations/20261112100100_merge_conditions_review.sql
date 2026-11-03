-- M3 R6 review (ADR-0024 R6):
--
-- 1. merge_settings.codeowners_restore_seen_at: when the worker last failed
--    to record an expired CODEOWNERS suspension. The worker scans the
--    oldest-seen first (NULLS FIRST), so one failing row never stays first.
--    Bookkeeping only: not an audited column (tables.toml
--    unaudited_columns), reset by every change of the switch.
--
-- 2. protected_ref_merge_rules_required_checks matches forge_core
--    CheckName::parse: `<ci|forge|kanflow>/<name>`, the name 1..=100 bytes
--    of a-z 0-9 . _ - /, starting with a letter or digit, no `//`, no
--    trailing `/` (the check names are space-separated for the match:
--    a name holds no space).

ALTER TABLE forge.merge_settings ADD COLUMN codeowners_restore_seen_at timestamptz;

ALTER TABLE forge.protected_ref_merge_rules
    DROP CONSTRAINT protected_ref_merge_rules_required_checks;
ALTER TABLE forge.protected_ref_merge_rules
    ADD CONSTRAINT protected_ref_merge_rules_required_checks
    CHECK (cardinality(required_checks) <= 20
        AND array_position(required_checks, NULL) IS NULL
        AND (cardinality(required_checks) = 0
            OR (' ' || array_to_string(required_checks, ' '))
                ~ '^( (ci|forge|kanflow)/(?=[a-z0-9._/-]{1,100}( |$))[a-z0-9][a-z0-9._-]*(/[a-z0-9._-]+)*)+$'));

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261112100100, "guarded_changes": ["forge.merge_settings", "forge.protected_ref_merge_rules"]}');
