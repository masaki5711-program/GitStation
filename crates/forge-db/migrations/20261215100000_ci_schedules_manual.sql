-- M4 C12 (plan 14 §5 C12; ADR-0029 Amendment 2026-11-13): manual jobs
-- and scheduled pipelines.
--
-- forge.ci_jobs (derived, as before):
--   manual    true for a job written `when: manual` in .forge-ci.yml: it is
--             created in status `manual` and runs only when a person starts
--             it (PipelineCreate, `pipeline.job_played` in the audit log).
--             Nothing waits for a manual job (forge-ci refuses `needs` on
--             one and leaves it out of the stage barrier), and a pipeline's
--             state is decided by its other jobs while it waits.
--   status    adds `manual` (waiting for a person); only a manual job may
--             be in it (CHECK ci_jobs_manual_status).
--
-- forge.ci_pipeline_schedules (audited on ('project', project_id)): a
-- project's timed pipelines, created, changed and deleted by a Maintainer
-- (PipelineScheduleManage: people only), audited in the same transaction
-- (`pipeline_schedule.created` / `.updated` / `.deleted`). Deleted with the
-- project in its purge.
--   description      what it is for (1..=255 bytes, one line)
--   ref              the branch whose tip is planned (`refs/heads/…`)
--   cron             five-field cron (forge_core::ci_schedule::Rule; at
--                    most once every 5 minutes), at `utc_offset_minutes`
--                    (fixed offset: Japan and Indonesia keep no DST)
--   active           false: never fires (next_run_at is NULL)
--   owner_id         the person whose rights it runs with: whoever last
--                    saved it. At each run the worker decides
--                    PipelineCreate for the owner as a signed-in person on
--                    the project and the account as they are then; a refusal
--                    starts nothing (FG-CI-0026). A scheduled pipeline is
--                    never `protected` (ci_pipelines_protected): no secret
--                    ever reaches its jobs (ADR-0029 obligations 5 and 7)
--   next_run_at      when it is due (the database's clock decides; the
--                    worker computes the next one from the rule)
--   last_run_at / last_pipeline_id / last_outcome / last_error_code
--                    what its last due run did: `created` (a pipeline),
--                    `no_definition`, `ref_missing`, `refused` (FG-CI-0026),
--                    `failed` (the code). Written by the worker only
--                    (unaudited columns; the run itself is audited as
--                    `pipeline.scheduled` when it starts a pipeline).

ALTER TABLE forge.ci_jobs ADD COLUMN manual boolean NOT NULL DEFAULT false;
ALTER TABLE forge.ci_jobs DROP CONSTRAINT ci_jobs_status;
ALTER TABLE forge.ci_jobs ADD CONSTRAINT ci_jobs_status CHECK (status IN
    ('created', 'pending', 'running', 'succeeded', 'failed', 'canceled', 'manual'));
ALTER TABLE forge.ci_jobs ADD CONSTRAINT ci_jobs_manual_status
    CHECK (status <> 'manual' OR manual);

CREATE TABLE forge.ci_pipeline_schedules (
    id                 bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    project_id         bigint NOT NULL REFERENCES forge.projects (id),
    description        text NOT NULL CONSTRAINT ci_pipeline_schedules_description CHECK (
        octet_length(description) BETWEEN 1 AND 255 AND description !~ '[[:cntrl:]]'),
    ref                text NOT NULL CONSTRAINT ci_pipeline_schedules_ref CHECK (
        ref LIKE 'refs/heads/_%' AND octet_length(ref) <= 255),
    cron               text NOT NULL CONSTRAINT ci_pipeline_schedules_cron CHECK (
        octet_length(cron) BETWEEN 9 AND 100),
    utc_offset_minutes integer NOT NULL CONSTRAINT ci_pipeline_schedules_offset CHECK (
        utc_offset_minutes BETWEEN -720 AND 840 AND utc_offset_minutes % 15 = 0),
    active             boolean NOT NULL DEFAULT true,
    owner_id           bigint NOT NULL REFERENCES forge.users (id),
    created_by         bigint NOT NULL REFERENCES forge.users (id),
    next_run_at        timestamptz,
    last_run_at        timestamptz,
    last_pipeline_id   bigint,
    last_outcome       text CONSTRAINT ci_pipeline_schedules_last_outcome CHECK (last_outcome IN
        ('created', 'no_definition', 'ref_missing', 'refused', 'failed')),
    last_error_code    text CONSTRAINT ci_pipeline_schedules_last_error_code
        CHECK (last_error_code ~ '^FG-[A-Z]+-[0-9]{4}$'),
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT ci_pipeline_schedules_next CHECK (active = (next_run_at IS NOT NULL))
);
CREATE INDEX ci_pipeline_schedules_project ON forge.ci_pipeline_schedules (project_id, id);
-- What the worker's claim reads.
CREATE INDEX ci_pipeline_schedules_due ON forge.ci_pipeline_schedules (next_run_at)
    WHERE active;
CREATE INDEX ci_pipeline_schedules_owner ON forge.ci_pipeline_schedules (owner_id);
CREATE INDEX ci_pipeline_schedules_created_by ON forge.ci_pipeline_schedules (created_by);

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, project_id, description, ref, cron,
        utc_offset_minutes, active, owner_id, created_by, created_at
    ON forge.ci_pipeline_schedules
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id',
        'updated_at,next_run_at,last_run_at,last_pipeline_id,last_outcome,last_error_code', '');
ALTER TABLE forge.ci_pipeline_schedules ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.ci_pipeline_schedules TO forge_app;
GRANT SELECT ON forge.ci_pipeline_schedules TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.ci_pipeline_schedules')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261215100000, "guarded_changes": ["forge.ci_pipeline_schedules.audited_change_present"]}');
