-- M4 C2 (plan 14 §4.1, ADR-0029 decision 5): CI pipelines and their jobs,
-- created from a push to a branch, a merge request version, or by hand.
--
-- DERIVED state (tables.toml kind = "derived"): a pipeline is what forge
-- planned from `.forge-ci.yml` at one commit (forge_ci::plan), and its jobs'
-- progress; forge_app writes it (the `ci.pipeline_create` job, later the
-- runner API) and deletes it in the project purge (projects/purge.rs).
-- Most writes are the worker's, with no person behind them, so the tables
-- carry no txn_audited trigger. What a PERSON does is audited by the command,
-- in the same transaction as the change (forge_db::pipelines, ChangeTx):
-- `pipeline.canceled`, `pipeline.retried`, `pipeline.run_manual` in
-- forge.audit_log (resource_type `project`, the actor and its credential).
--
-- Retention (plan 14 §4.1): a pipeline and its jobs are kept 90 days, like
-- the job logs; the purge job comes with the logs (C5) and reads by
-- `created_at` (it adds its own index then). Nothing is deleted before that
-- except by the project purge.
--
-- forge.ci_pipelines
--   ref              the branch (`refs/heads/…`): the pushed one, a merge
--                    request's source branch, or the manual run's
--   sha              the commit planned (lowercase hex, 40 or 64 digits;
--                    compared as text, never as a short id: ADR-0029 C3 #3)
--   source           push | merge_request | schedule | manual
--   merge_request_id the merge request (merge_request only)
--   triggered_by     the pusher / the person (NULL: a version without pusher)
--   definition_hash  sha256 (hex) of the `.forge-ci.yml` bytes planned
--                    (NULL when no bytes were read: `error`, or an
--                    `invalid` entry that is no file or is too large)
--   protected        true only for a push to a branch that was protected when
--                    the pipeline was created. Secrets may be released to its
--                    jobs from this column and `source = 'push'` alone
--                    (ADR-0029, obligation of C3/C6); never to a
--                    merge_request, manual or scheduled pipeline, which the
--                    CHECK `ci_pipelines_protected` makes unrepresentable
--   copy_mark        the confidential marks at `sha`, measured at creation
--                    (open | confidential | undecided; ADR-0029 decision 5:
--                    the CI outputs inherit it, C5)
--   status           pending | running | succeeded | failed | canceled
--                    | skipped (no job selected) | invalid (FG-CI-0001)
--                    | error (FG-CI-0002: the definition could not be read)
--   error_code / problems
--                    why `invalid` / `error`: the code and, for `invalid`,
--                    at most 20 problems {line, reason, subject}
--
-- One pipeline per (project, source, ref, sha, merge request) for push and
-- merge_request (`ci_pipelines_once`): a redelivered event or a retried job
-- never creates a second one. Manual and scheduled pipelines are not
-- deduplicated (each request is a run of its own).
--
-- forge.ci_jobs: one row per planned job (forge_ci::PlannedJob).
--   needs            the jobs it waits for (the stage wall and `needs`)
--   status           created (waits for `needs`) | pending (ready for a
--                    runner) | running | succeeded | failed | canceled
--   attempt          1, +1 on each retry (the row is reused)
--   runner_id / lease_until
--                    the runner holding it (M4 C3; no foreign key until
--                    forge.ci_runners exists)

CREATE TABLE forge.ci_pipelines (
    id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    project_id       bigint NOT NULL REFERENCES forge.projects (id),
    ref              text NOT NULL CONSTRAINT ci_pipelines_ref CHECK (
        ref LIKE 'refs/heads/_%' AND octet_length(ref) <= 255),
    sha              text NOT NULL CONSTRAINT ci_pipelines_sha
        CHECK (sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    source           text NOT NULL CONSTRAINT ci_pipelines_source
        CHECK (source IN ('push', 'merge_request', 'schedule', 'manual')),
    merge_request_id bigint REFERENCES forge.merge_requests (id),
    triggered_by     bigint REFERENCES forge.users (id),
    definition_hash  text CONSTRAINT ci_pipelines_definition_hash
        CHECK (definition_hash ~ '^[0-9a-f]{64}$'),
    protected        boolean NOT NULL DEFAULT false,
    copy_mark        text NOT NULL CONSTRAINT ci_pipelines_copy_mark
        CHECK (copy_mark IN ('open', 'confidential', 'undecided')),
    status           text NOT NULL CONSTRAINT ci_pipelines_status CHECK (status IN
        ('pending', 'running', 'succeeded', 'failed', 'canceled', 'skipped', 'invalid', 'error')),
    error_code       text CONSTRAINT ci_pipelines_error_code
        CHECK (error_code ~ '^FG-[A-Z]+-[0-9]{4}$'),
    problems         jsonb NOT NULL DEFAULT '[]'::jsonb CONSTRAINT ci_pipelines_problems
        CHECK (jsonb_typeof(problems) = 'array' AND jsonb_array_length(problems) <= 20
            AND octet_length(problems::text) <= 16384),
    request_id       text NOT NULL CONSTRAINT ci_pipelines_request_id
        CHECK (octet_length(request_id) BETWEEN 1 AND 128),
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    finished_at      timestamptz,
    CONSTRAINT ci_pipelines_merge_request
        CHECK ((source = 'merge_request') = (merge_request_id IS NOT NULL)),
    CONSTRAINT ci_pipelines_error
        CHECK ((status IN ('invalid', 'error')) = (error_code IS NOT NULL)),
    CONSTRAINT ci_pipelines_definition
        CHECK (status <> 'error' OR definition_hash IS NULL),
    CONSTRAINT ci_pipelines_protected
        CHECK (NOT protected OR source = 'push')
);
-- Exactly once per event (module comment).
CREATE UNIQUE INDEX ci_pipelines_once ON forge.ci_pipelines
    (project_id, source, ref, sha, coalesce(merge_request_id, 0))
    WHERE source IN ('push', 'merge_request');
-- A project's pipelines, newest first (the list route).
CREATE INDEX ci_pipelines_project ON forge.ci_pipelines (project_id, id DESC);
CREATE INDEX ci_pipelines_merge_request ON forge.ci_pipelines (merge_request_id)
    WHERE merge_request_id IS NOT NULL;
CREATE INDEX ci_pipelines_triggered_by ON forge.ci_pipelines (triggered_by)
    WHERE triggered_by IS NOT NULL;

CREATE TABLE forge.ci_jobs (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    pipeline_id  bigint NOT NULL REFERENCES forge.ci_pipelines (id),
    project_id   bigint NOT NULL REFERENCES forge.projects (id),
    name         text NOT NULL CONSTRAINT ci_jobs_name
        CHECK (name ~ '^[a-z0-9][a-z0-9._-]{0,62}$'),
    stage        text NOT NULL CONSTRAINT ci_jobs_stage
        CHECK (stage ~ '^[a-z0-9][a-z0-9._-]{0,62}$'),
    wave         integer NOT NULL CONSTRAINT ci_jobs_wave CHECK (wave BETWEEN 0 AND 48),
    needs        text[] NOT NULL DEFAULT '{}' CONSTRAINT ci_jobs_needs
        CHECK (cardinality(needs) <= 48),
    image        text NOT NULL CONSTRAINT ci_jobs_image
        CHECK (image LIKE '%@sha256:%' AND octet_length(image) <= 512),
    timeout_s    integer NOT NULL CONSTRAINT ci_jobs_timeout CHECK (timeout_s BETWEEN 1 AND 14400),
    tags         text[] NOT NULL DEFAULT '{}' CONSTRAINT ci_jobs_tags
        CHECK (cardinality(tags) <= 8),
    secrets      text[] NOT NULL DEFAULT '{}' CONSTRAINT ci_jobs_secrets
        CHECK (cardinality(secrets) <= 16),
    status       text NOT NULL CONSTRAINT ci_jobs_status CHECK (status IN
        ('created', 'pending', 'running', 'succeeded', 'failed', 'canceled')),
    attempt      integer NOT NULL DEFAULT 1 CONSTRAINT ci_jobs_attempt CHECK (attempt BETWEEN 1 AND 100),
    runner_id    bigint,
    lease_until  timestamptz,
    error_code   text CONSTRAINT ci_jobs_error_code CHECK (error_code ~ '^FG-[A-Z]+-[0-9]{4}$'),
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    started_at   timestamptz,
    finished_at  timestamptz,
    CONSTRAINT ci_jobs_one_name UNIQUE (pipeline_id, name),
    CONSTRAINT ci_jobs_lease CHECK ((runner_id IS NULL) = (lease_until IS NULL))
);
-- The project purge deletes a project's rows.
CREATE INDEX ci_jobs_project ON forge.ci_jobs (project_id);
-- What a runner may claim (M4 C3).
CREATE INDEX ci_jobs_pending ON forge.ci_jobs (id) WHERE status = 'pending';

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.ci_pipelines TO forge_app;
GRANT SELECT ON forge.ci_pipelines TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.ci_jobs TO forge_app;
GRANT SELECT ON forge.ci_jobs TO forge_ro;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261204100000, "guarded_changes": []}');
