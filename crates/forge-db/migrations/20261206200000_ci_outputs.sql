-- M4 C5 (plan 14 §4.1, §5 C5; plan 04 P-6 / P-7 / P-9; ADR-0029 decisions 5
-- and 7): the CI outputs — failure fingerprints and the diagnosis, the job
-- log's guarded read, and the `ci/pipeline` check forge's worker writes.
--
-- forge.failure_fingerprints (derived): one row per (project, fingerprint).
-- A fingerprint (`fp_` + 16 hex) is sha256 of a failed job's failure lines
-- with what varies between runs removed (times, durations, hex ids,
-- numbers, temporary paths; forge-pipelines `fingerprint`), so one failure
-- seen again folds into the same row (`occurrences + 1`, `last_*`).
--   first_job_id / last_job_id
--                the first and the latest failed job that had it (SET NULL
--                when the job is deleted by the retention)
--   fixed_by_merge_request_id / fixed_at
--                plan 04 P-7, deterministic: the first merge request whose
--                pipeline failed with it and whose later pipeline succeeded
--                (forge_db::ci_outputs, the `ci/pipeline` sweep). The
--                diagnosis returns it as a known fix.
-- No log text is kept here.
--
-- forge.ci_job_failures (derived): one row per failed job attempt — its
-- fingerprint, the error codes its log names (FG-…, at most 10), and the
-- excerpt: the failure lines, MASKED (forge-pipelines `mask`) and bounded
-- (8 KiB), with their line numbers in the attempt's log. The excerpt is
-- log output: it inherits the pipeline's marks and is read only through
-- PipelineDiagnosisRead (FG-AUTH-0022 for an agent audience on a marked
-- pipeline or tip). The read pool sees no excerpt (as no log body).
--
-- forge.ci_jobs.diagnosed_attempt: the attempt the fingerprint sweep last
-- recorded (a failed job whose attempt differs is still to fingerprint).
--
-- forge.ci_pipelines.check_status: the pipeline status last written as the
-- `ci/pipeline` check of its commit (the sweep writes a pipeline whose
-- status differs). Only the newest pipeline of a (project, commit) writes
-- the check; an older one is only marked.
--
-- forge.check_runs: `ci/pipeline` is written by forge's worker with no
-- reporter (like `forge/`). check_runs_reporter now allows a NULL reporter
-- for `ci/pipeline` too, and still requires it for every other `ci/` name.
-- A `ci/pipeline` row a service account reported before C5 (no rule stopped
-- it then) keeps its reporter until the worker overwrites it, so the
-- constraint allows both shapes for that one name.
--
-- forge.ci_log_chunks: unchanged. The read pool still never sees `body`;
-- the only read of it is forge_db::ci_outputs on the write pool, after
-- authorize_ci_output (or the system's own fingerprint sweep).

CREATE TABLE forge.failure_fingerprints (
    id                        bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    project_id                bigint NOT NULL REFERENCES forge.projects (id),
    fingerprint               text NOT NULL CONSTRAINT failure_fingerprints_fingerprint
        CHECK (fingerprint ~ '^fp_[0-9a-f]{16}$'),
    first_job_id              bigint REFERENCES forge.ci_jobs (id) ON DELETE SET NULL,
    last_job_id               bigint REFERENCES forge.ci_jobs (id) ON DELETE SET NULL,
    first_seen_at             timestamptz NOT NULL DEFAULT now(),
    last_seen_at              timestamptz NOT NULL DEFAULT now(),
    occurrences               integer NOT NULL DEFAULT 1 CONSTRAINT failure_fingerprints_occurrences
        CHECK (occurrences >= 1),
    fixed_by_merge_request_id bigint REFERENCES forge.merge_requests (id),
    fixed_at                  timestamptz,
    CONSTRAINT failure_fingerprints_once UNIQUE (project_id, fingerprint),
    CONSTRAINT failure_fingerprints_fixed
        CHECK ((fixed_by_merge_request_id IS NULL) = (fixed_at IS NULL))
);
CREATE INDEX failure_fingerprints_first_job ON forge.failure_fingerprints (first_job_id);
CREATE INDEX failure_fingerprints_last_job ON forge.failure_fingerprints (last_job_id);
CREATE INDEX failure_fingerprints_fixed_by ON forge.failure_fingerprints (fixed_by_merge_request_id)
    WHERE fixed_by_merge_request_id IS NOT NULL;

CREATE TABLE forge.ci_job_failures (
    job_id          bigint NOT NULL REFERENCES forge.ci_jobs (id),
    attempt         integer NOT NULL CONSTRAINT ci_job_failures_attempt
        CHECK (attempt BETWEEN 1 AND 100),
    project_id      bigint NOT NULL REFERENCES forge.projects (id),
    pipeline_id     bigint NOT NULL REFERENCES forge.ci_pipelines (id),
    fingerprint_id  bigint NOT NULL REFERENCES forge.failure_fingerprints (id),
    error_codes     text[] NOT NULL DEFAULT '{}' CONSTRAINT ci_job_failures_error_codes
        CHECK (cardinality(error_codes) <= 10 AND array_position(error_codes, NULL) IS NULL),
    excerpt         text NOT NULL CONSTRAINT ci_job_failures_excerpt
        CHECK (octet_length(excerpt) <= 8192),
    first_line      integer CONSTRAINT ci_job_failures_first_line CHECK (first_line >= 1),
    last_line       integer,
    created_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (job_id, attempt),
    CONSTRAINT ci_job_failures_lines
        CHECK ((first_line IS NULL) = (last_line IS NULL) AND last_line >= first_line)
);
CREATE INDEX ci_job_failures_pipeline ON forge.ci_job_failures (pipeline_id);
CREATE INDEX ci_job_failures_project ON forge.ci_job_failures (project_id);
CREATE INDEX ci_job_failures_fingerprint ON forge.ci_job_failures (fingerprint_id);

ALTER TABLE forge.ci_jobs
    ADD COLUMN diagnosed_attempt integer CONSTRAINT ci_jobs_diagnosed_attempt
        CHECK (diagnosed_attempt BETWEEN 1 AND 100);
-- The fingerprint sweep's scan: failed jobs not fingerprinted at their attempt.
CREATE INDEX ci_jobs_undiagnosed ON forge.ci_jobs (id)
    WHERE status = 'failed' AND diagnosed_attempt IS DISTINCT FROM attempt;

ALTER TABLE forge.ci_pipelines
    ADD COLUMN check_status text CONSTRAINT ci_pipelines_check_status CHECK (check_status IN
        ('pending', 'running', 'succeeded', 'failed', 'canceled', 'skipped', 'invalid', 'error'));
-- The check sweep's scan: pipelines whose status is not yet the check's.
CREATE INDEX ci_pipelines_check_due ON forge.ci_pipelines (id)
    WHERE check_status IS DISTINCT FROM status;
-- The newest pipeline of a commit (only it writes the check).
CREATE INDEX ci_pipelines_commit ON forge.ci_pipelines (project_id, sha, id DESC);

ALTER TABLE forge.check_runs DROP CONSTRAINT check_runs_reporter;
ALTER TABLE forge.check_runs ADD CONSTRAINT check_runs_reporter
    CHECK ((name LIKE 'forge/%' AND reported_by IS NULL)
        OR (name = 'ci/pipeline')
        OR (name NOT LIKE 'forge/%' AND reported_by IS NOT NULL))
    NOT VALID;
ALTER TABLE forge.check_runs VALIDATE CONSTRAINT check_runs_reporter;
ALTER TABLE forge.check_runs ADD CONSTRAINT check_runs_reporter_credential
    CHECK (reported_by IS NOT NULL OR credential_id IS NULL) NOT VALID;
ALTER TABLE forge.check_runs VALIDATE CONSTRAINT check_runs_reporter_credential;

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.failure_fingerprints TO forge_app;
GRANT SELECT ON forge.failure_fingerprints TO forge_ro;
-- Append only per attempt: no UPDATE. No excerpt for the read pool.
GRANT SELECT, INSERT, DELETE ON forge.ci_job_failures TO forge_app;
GRANT SELECT (job_id, attempt, project_id, pipeline_id, fingerprint_id, error_codes, first_line,
    last_line, created_at) ON forge.ci_job_failures TO forge_ro;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261206200000, "guarded_changes": []}');
