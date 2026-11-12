-- M4 C6 review (security + ci-runner-security, 2026-11-12): what a job was
-- given, recorded at its claim.
--
-- forge.ci_jobs.secrets_released: true once any attempt of the job was
-- handed secret values (sticky). Its artifacts are then scanned for the
-- masked values at upload (FG-CI-0021), readable only by Maintainers who
-- are people (never an AI agent), and given to a `needs` consumer only in
-- a protected push pipeline.
--
-- forge.ci_job_secret_snapshots (derived): the sealed values exactly as
-- they were released to one attempt (key_id, ciphertext: the same blob as
-- forge.ci_variables then, bound to (project, name)), so the log and the
-- artifacts stay masked for the released value even if the variable is
-- changed or deleted while the job runs. Written in one transaction with
-- the flag; deleted with the job (project purge). The read pool never
-- sees the ciphertext (ADR-0018).

ALTER TABLE forge.ci_jobs
    ADD COLUMN secrets_released boolean NOT NULL DEFAULT false;

CREATE TABLE forge.ci_job_secret_snapshots (
    job_id       bigint NOT NULL REFERENCES forge.ci_jobs (id),
    attempt      integer NOT NULL CONSTRAINT ci_job_secret_snapshots_attempt
        CHECK (attempt BETWEEN 1 AND 100),
    project_id   bigint NOT NULL REFERENCES forge.projects (id),
    name         text NOT NULL CONSTRAINT ci_job_secret_snapshots_name
        CHECK (name ~ '^[A-Z][A-Z0-9_]{0,62}$'),
    key_id       smallint NOT NULL CONSTRAINT ci_job_secret_snapshots_key_id CHECK (key_id >= 1),
    ciphertext   bytea NOT NULL CONSTRAINT ci_job_secret_snapshots_ciphertext
        CHECK (octet_length(ciphertext) BETWEEN 29 AND 4124),
    masked       boolean NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (job_id, attempt, name)
);
CREATE INDEX ci_job_secret_snapshots_project ON forge.ci_job_secret_snapshots (project_id);

GRANT SELECT, INSERT, DELETE ON forge.ci_job_secret_snapshots TO forge_app;
GRANT SELECT (job_id, attempt, project_id, name, key_id, masked, created_at)
    ON forge.ci_job_secret_snapshots TO forge_ro;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261211300000, "guarded_changes": []}');
