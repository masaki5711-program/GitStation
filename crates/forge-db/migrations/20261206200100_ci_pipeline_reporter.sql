-- M4 C5 (ADR-0029 decision 7, review C5): `ci/pipeline` is written by
-- forge's worker with no reporter, like `forge/`. 20261206200000 removed
-- every `ci/pipeline` run an account reported before C5 (audited per
-- project as `check_run.reserved_removed`); in its own transaction, so
-- the audit checks of those deletes have run before this ALTER TABLE.
-- check_runs_reporter now requires no reporter (and no credential)
-- exactly for `forge/` names and `ci/pipeline`, and a reporter for every
-- other name. Locking: DROP / ADD CONSTRAINT take an ACCESS EXCLUSIVE
-- lock; VALIDATE scans the table once (small: at most 100 names per
-- commit, kept by the check_run_retention task).

ALTER TABLE forge.check_runs DROP CONSTRAINT check_runs_reporter;
ALTER TABLE forge.check_runs ADD CONSTRAINT check_runs_reporter
    CHECK ((reported_by IS NULL) = (name LIKE 'forge/%' OR name = 'ci/pipeline')
        AND (reported_by IS NOT NULL OR credential_id IS NULL))
    NOT VALID;
ALTER TABLE forge.check_runs VALIDATE CONSTRAINT check_runs_reporter;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261206200100, "guarded_changes": []}');
