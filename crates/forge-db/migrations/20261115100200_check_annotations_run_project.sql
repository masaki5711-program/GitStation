-- M3 C1 review fix 4 (database MEDIUM): an annotation's project_id is its
-- run's project. check_annotations carries project_id for the audit
-- trigger (resource `project`) and the purge's delete; without a link to
-- its run's project the two could disagree (an annotation audited and
-- purged with another project than its run). check_runs gains
-- UNIQUE (id, project_id) (check_runs_id_project) so that
-- check_annotations can reference (check_run_id, project_id)
-- (check_annotations_run_project). The tables are small (C1 ships them),
-- so the constraints are added at once.

ALTER TABLE forge.check_runs
    ADD CONSTRAINT check_runs_id_project UNIQUE (id, project_id);
ALTER TABLE forge.check_annotations
    ADD CONSTRAINT check_annotations_run_project
    FOREIGN KEY (check_run_id, project_id) REFERENCES forge.check_runs (id, project_id);

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261115100200, "guarded_changes": []}');
