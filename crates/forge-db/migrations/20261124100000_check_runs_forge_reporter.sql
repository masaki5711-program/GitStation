-- M3 X2 (ADR-0023 Amendment X2, ADR-0024 §10): forge's own checks.
--
-- `forge/` check runs are written by forge's worker, never over HTTP (the
-- CheckWrite route refuses `forge/` to everyone). forge's worker is no
-- account, so such a run has no reporter: reported_by (and credential_id)
-- is NULL exactly for a `forge/` name, and a run of every other namespace
-- keeps its reporting account (check_runs_reporter). No `forge/` row can
-- exist before this migration (no path wrote one), so the CHECK holds for
-- every existing row; the table is small at M3 (at most 100 names per
-- commit) and is validated at once. The guard check_runs_guard is
-- unchanged (a run's name never changes, so a run never moves between the
-- two shapes). The table stays audited on the project.

ALTER TABLE forge.check_runs ALTER COLUMN reported_by DROP NOT NULL;
ALTER TABLE forge.check_runs ADD CONSTRAINT check_runs_reporter
    CHECK ((reported_by IS NULL) = (name LIKE 'forge/%')
        AND (reported_by IS NOT NULL OR credential_id IS NULL));

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261124100000, "guarded_changes": []}');
