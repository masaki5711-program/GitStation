-- M3 R6 review round 2: protected_ref_merge_rules_required_checks matched
-- the space-joined names (20261112100100), so one element holding two
-- names (`'ci/a ci/b'`) passed — and then no reported check could ever
-- match it, blocking every merge into the branch. The CHECK now also
-- requires the joined names to split back into exactly as many names as
-- the array has elements: no element may hold a space (every other
-- whitespace is already outside the name's characters). No subquery: the
-- cardinality comparison is a plain expression.

ALTER TABLE forge.protected_ref_merge_rules
    DROP CONSTRAINT protected_ref_merge_rules_required_checks;
ALTER TABLE forge.protected_ref_merge_rules
    ADD CONSTRAINT protected_ref_merge_rules_required_checks
    CHECK (cardinality(required_checks) <= 20
        AND array_position(required_checks, NULL) IS NULL
        AND (cardinality(required_checks) = 0
            OR (cardinality(string_to_array(array_to_string(required_checks, ' '), ' '))
                    = cardinality(required_checks)
                AND (' ' || array_to_string(required_checks, ' '))
                    ~ '^( (ci|forge|kanflow)/(?=[a-z0-9._/-]{1,100}( |$))[a-z0-9][a-z0-9._-]*(/[a-z0-9._-]+)*)+$')));

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261112100300, "guarded_changes": ["forge.protected_ref_merge_rules"]}');
