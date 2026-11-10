-- M4 C3 (plan 14 §4.1, §5 C3, ADR-0029): the runner API — registered
-- runners, the short-lived job tokens they are handed with each job, and
-- the job log the runners append.
--
-- forge.ci_runners (audited on ('ci_runner', id)): a runner an
-- administrator registered (RunnerRegister: an administrator's web
-- session, people only; `ci_runner.registered`) and changes
-- (RunnerManage: `ci_runner.updated` — enabled, tags, description).
--   public_id / secret_sha256
--                the token `fgr_<public_id>_<secret>` (ADR-0019's shape):
--                only sha256(secret) is stored; the read pool never sees
--                it (ADR-0018). Shown once, at registration.
--   tags         a job runs on a runner whose tags cover every tag the
--                job asks for (`tags:` in .forge-ci.yml); an untagged job
--                runs on any runner. Lets confidential repositories keep
--                runners of their own (ADR-0029 risks).
--   enabled      a disabled runner's token is refused (FG-AUTH-0001
--                reason=runner_disabled) and it claims nothing.
--   last_contact_at
--                the last runner API call (unaudited: written by the
--                runner's own requests, at most every 30 seconds).
-- Runners are never deleted (disable them): their ids stay in jobs.
--
-- forge.ci_job_tokens (derived): one token per job attempt, issued with the
-- claim (`fgj_<public_id>_<secret>`, sha256 only), bound to the job's
-- project and pinned SHA (ADR-0029 §1, §3). Checked against this table on
-- every request (never a signature-only token):
--   expires_at   claim time + the job's timeout + 10 minutes, compared
--                with the database's now() (ADR-0029 obligation 2)
--   revoked_at / revoked_reason
--                written in the same transaction as the job's end
--                (job_finished, job_canceled, lease_lost) and as the
--                project's deletion or move (project_deleted,
--                project_moved; obligation 1). Final.
-- At most one live token per job (`ci_job_tokens_live`).
--
-- forge.ci_log_chunks (derived, append only): what a runner appended for a
-- job attempt, in `seq` order (0-based; a resent chunk with the same seq
-- is a no-op). At most 64 KiB per chunk, 4096 chunks and 16 MiB per
-- attempt (FG-CI-0006). forge_app may INSERT and DELETE (the purge), never
-- UPDATE. **No read path exists in C3**: the read pool sees no `body`, and
-- the reads that decide the CI outputs on their marks (FG-AUTH-0022,
-- ADR-0029 decision 5) come with C5 (test
-- `ci_logs_have_no_read_path_before_c5`). Kept 90 days with the pipelines
-- (the purge job is C5's).
--
-- forge.ci_jobs gains the foreign key on runner_id and lease_losses (the
-- reaper's count: a third lost lease fails the job with FG-CI-0004).

CREATE TABLE forge.ci_runners (
    id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    public_id        text NOT NULL CONSTRAINT ci_runners_public_id
        CHECK (public_id ~ '^[0-9a-f]{16}$'),
    secret_sha256    bytea NOT NULL CONSTRAINT ci_runners_secret
        CHECK (octet_length(secret_sha256) = 32),
    name             text NOT NULL CONSTRAINT ci_runners_name
        CHECK (name ~ '^[a-z0-9][a-z0-9._-]{0,62}$'),
    description      text NOT NULL DEFAULT '' CONSTRAINT ci_runners_description
        CHECK (octet_length(description) <= 1024),
    tags             text[] NOT NULL DEFAULT '{}' CONSTRAINT ci_runners_tags
        CHECK (cardinality(tags) <= 16 AND array_position(tags, NULL) IS NULL),
    enabled          boolean NOT NULL DEFAULT true,
    created_by       bigint NOT NULL REFERENCES forge.users (id),
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    last_contact_at  timestamptz,
    CONSTRAINT ci_runners_public_id_unique UNIQUE (public_id),
    CONSTRAINT ci_runners_name_unique UNIQUE (name)
);
CREATE INDEX ci_runners_created_by ON forge.ci_runners (created_by);

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, public_id, secret_sha256, name, description, tags,
        enabled, created_by, created_at
    ON forge.ci_runners
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('ci_runner', 'id', 'updated_at,last_contact_at', '');
ALTER TABLE forge.ci_runners ENABLE ALWAYS TRIGGER audited_change_present;

ALTER TABLE forge.ci_jobs
    ADD CONSTRAINT ci_jobs_runner FOREIGN KEY (runner_id) REFERENCES forge.ci_runners (id),
    ADD COLUMN lease_losses integer NOT NULL DEFAULT 0
        CONSTRAINT ci_jobs_lease_losses CHECK (lease_losses BETWEEN 0 AND 3);
-- The reaper's scan: held jobs by lease end.
CREATE INDEX ci_jobs_leased ON forge.ci_jobs (lease_until) WHERE runner_id IS NOT NULL;

CREATE TABLE forge.ci_job_tokens (
    id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    public_id        text NOT NULL CONSTRAINT ci_job_tokens_public_id
        CHECK (public_id ~ '^[0-9a-f]{16}$'),
    secret_sha256    bytea NOT NULL CONSTRAINT ci_job_tokens_secret
        CHECK (octet_length(secret_sha256) = 32),
    job_id           bigint NOT NULL REFERENCES forge.ci_jobs (id),
    attempt          integer NOT NULL CONSTRAINT ci_job_tokens_attempt
        CHECK (attempt BETWEEN 1 AND 100),
    runner_id        bigint NOT NULL REFERENCES forge.ci_runners (id),
    project_id       bigint NOT NULL REFERENCES forge.projects (id),
    sha              text NOT NULL CONSTRAINT ci_job_tokens_sha
        CHECK (sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    created_at       timestamptz NOT NULL DEFAULT now(),
    expires_at       timestamptz NOT NULL,
    revoked_at       timestamptz,
    revoked_reason   text CONSTRAINT ci_job_tokens_revoked_reason CHECK (revoked_reason IN
        ('job_finished', 'job_canceled', 'lease_lost', 'project_deleted', 'project_moved')),
    CONSTRAINT ci_job_tokens_public_id_unique UNIQUE (public_id),
    CONSTRAINT ci_job_tokens_revoked CHECK ((revoked_at IS NULL) = (revoked_reason IS NULL)),
    CONSTRAINT ci_job_tokens_expiry CHECK (expires_at > created_at)
);
-- One live token per job.
CREATE UNIQUE INDEX ci_job_tokens_live ON forge.ci_job_tokens (job_id) WHERE revoked_at IS NULL;
CREATE INDEX ci_job_tokens_job ON forge.ci_job_tokens (job_id);
-- Revoked with the project (deletion, move) and deleted in its purge.
CREATE INDEX ci_job_tokens_project ON forge.ci_job_tokens (project_id);
CREATE INDEX ci_job_tokens_runner ON forge.ci_job_tokens (runner_id);

CREATE TABLE forge.ci_log_chunks (
    job_id           bigint NOT NULL REFERENCES forge.ci_jobs (id),
    attempt          integer NOT NULL CONSTRAINT ci_log_chunks_attempt
        CHECK (attempt BETWEEN 1 AND 100),
    seq              integer NOT NULL CONSTRAINT ci_log_chunks_seq CHECK (seq BETWEEN 0 AND 4095),
    project_id       bigint NOT NULL REFERENCES forge.projects (id),
    body             text NOT NULL CONSTRAINT ci_log_chunks_body
        CHECK (octet_length(body) BETWEEN 1 AND 65536),
    created_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (job_id, attempt, seq)
);
CREATE INDEX ci_log_chunks_project ON forge.ci_log_chunks (project_id);

GRANT SELECT, INSERT ON forge.ci_runners TO forge_app;
GRANT UPDATE (description, tags, enabled, updated_at, last_contact_at) ON forge.ci_runners TO forge_app;
-- Not secret_sha256 (ADR-0018: the read pool never sees token hashes).
GRANT SELECT (id, public_id, name, description, tags, enabled, created_by, created_at, updated_at,
    last_contact_at) ON forge.ci_runners TO forge_ro;

GRANT SELECT, INSERT, DELETE ON forge.ci_job_tokens TO forge_app;
GRANT UPDATE (revoked_at, revoked_reason) ON forge.ci_job_tokens TO forge_app;
GRANT SELECT (id, public_id, job_id, attempt, runner_id, project_id, sha, created_at, expires_at,
    revoked_at, revoked_reason) ON forge.ci_job_tokens TO forge_ro;

-- Append only: no UPDATE. No body for the read pool before C5.
GRANT SELECT, INSERT, DELETE ON forge.ci_log_chunks TO forge_app;
GRANT SELECT (job_id, attempt, seq, project_id, created_at) ON forge.ci_log_chunks TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.ci_runners')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261205100000, "guarded_changes": ["forge.ci_runners.audited_change_present"]}');
