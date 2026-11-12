-- M4 C6 (plan 14 §4.1, §9 #6; ADR-0029 decision 5): CI artifacts — the
-- files a job declared under `artifacts: { paths }` and its runner uploaded
-- with the job's token after the script succeeded.
--
-- DERIVED state (tables.toml kind = "derived"): written by a job's upload
-- (no person behind it) and deleted by the worker's expiry sweep and the
-- project purge; no audit row.
--   job_id / attempt  the job attempt that uploaded it (a retry uploads its
--                     own; a later job reads the current attempt's)
--   path              the file's path relative to the work tree
--                     (forge_core::ci_artifact::check_path)
--   size_bytes / sha256
--                     measured by forge while it stored the bytes
--   (the file)        `<storage.root>/ci-artifacts/<project_id>/<id>`: the
--                     server names it from the ids, never from the path
--   expires_at        created_at + 30 days; the sweep deletes the file, then
--                     the row. `keep` (a deployed version, C11) never expires.
-- At most one row per (job, attempt, path): an upload of the same path again
-- is refused (FG-WEB-0006 reason=artifact_exists).

CREATE TABLE forge.ci_artifacts (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    project_id   bigint NOT NULL REFERENCES forge.projects (id),
    pipeline_id  bigint NOT NULL REFERENCES forge.ci_pipelines (id),
    job_id       bigint NOT NULL REFERENCES forge.ci_jobs (id),
    attempt      integer NOT NULL CONSTRAINT ci_artifacts_attempt CHECK (attempt BETWEEN 1 AND 100),
    path         text NOT NULL CONSTRAINT ci_artifacts_path CHECK (
        octet_length(path) BETWEEN 1 AND 1024 AND path NOT LIKE '/%'),
    size_bytes   bigint NOT NULL CONSTRAINT ci_artifacts_size
        CHECK (size_bytes BETWEEN 0 AND 104857600),
    sha256       bytea NOT NULL CONSTRAINT ci_artifacts_sha256 CHECK (octet_length(sha256) = 32),
    keep         boolean NOT NULL DEFAULT false,
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL,
    CONSTRAINT ci_artifacts_one_path UNIQUE (job_id, attempt, path),
    CONSTRAINT ci_artifacts_expiry CHECK (expires_at > created_at)
);
CREATE INDEX ci_artifacts_project ON forge.ci_artifacts (project_id);
CREATE INDEX ci_artifacts_pipeline ON forge.ci_artifacts (pipeline_id);
-- The expiry sweep.
CREATE INDEX ci_artifacts_expiring ON forge.ci_artifacts (expires_at) WHERE NOT keep;

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.ci_artifacts TO forge_app;
GRANT SELECT ON forge.ci_artifacts TO forge_ro;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261211200000, "guarded_changes": []}');
