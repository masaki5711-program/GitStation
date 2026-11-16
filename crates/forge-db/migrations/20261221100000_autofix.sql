-- M4 C14 (plan 14 §5 C14; ADR-0017 §1, §2 and its Amendment C14; plan 04
-- C-5): the deterministic autofix — its project switch and its attempts.
--
-- DERIVED state (tables.toml kind = "derived"). What a PERSON does — turn a
-- project's autofix on or off — is audited by the command in the same
-- transaction (forge_db::autofix, ChangeTx: `autofix.settings_changed`,
-- resource the project). What the bot does is recorded as the merge
-- request's event `merge_request.autofix_finished` (actor system) and, for
-- a commit it pushed, as the push (`repository.pushed`).
--
-- forge.project_autofix: a project's `autofix.enabled`. A project WITHOUT a
-- row is OFF (the default of every project, old and new: the user turns it
-- on, a Maintainer, plan 14 C14).
--   enabled      on / off
--   changed_by   who turned it on or off last: the ENABLER, whose role the
--                bot's every push is decided with (ADR-0017 §2.5: bot ∩
--                author ∩ enabler)
--
-- forge.autofix_attempts: one fixer run for one merge request at one head.
--   trigger      pipeline_failed (the merge result's ci/pipeline failed) |
--                conflict (merge_request.conflict_detected)
--   head_sha / target_sha   the merge request's head and target it started
--                from (the preconditions: the bot's commit lands only while
--                the source branch is still at head_sha)
--   fixer        the target branch's `autofix.fixers.<name>` (NULL while
--                queued: chosen when it starts)
--   state        queued | running | applied | no_change | refused |
--                proposed | failed | skipped | superseded
--   reason       a stable word (why it was refused, proposed, skipped, ...)
--   detail       the paths behind a proposal or a refusal (at most 20)
--   input_sha    the input commit the runner checked out (the head's tree,
--                or the server's conflicted merge, with the target branch's
--                `.forge-ci.yml`; parents head [, target])
--   pipeline_id / job_id   the fixer's own pipeline (source `autofix`)
--   commit_sha   the bot's commit (recorded just BEFORE the source branch
--                is moved, so the version job never carries an approval
--                onto it; set on every applied attempt): the truth of "this
--                commit is the bot's" (trailers can be copied by anyone)
-- One row per (merge request, trigger, head, target, fixer)
-- (`autofix_attempts_once`; the trigger's row has fixer NULL): a repeated
-- event never starts a fixer twice on the same head.
--
-- forge.ci_pipelines gains source 'autofix' (the fixer's pipeline: ref = the
-- merge request's source branch, no merge request row link — it is no
-- pipeline OF the merge request —, never protected, never deduplicated).

CREATE TABLE forge.project_autofix (
    project_id  bigint PRIMARY KEY REFERENCES forge.projects (id),
    enabled     boolean NOT NULL,
    changed_by  bigint NOT NULL REFERENCES forge.users (id),
    changed_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX project_autofix_changed_by ON forge.project_autofix (changed_by);

CREATE TABLE forge.autofix_attempts (
    id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    project_id        bigint NOT NULL REFERENCES forge.projects (id),
    merge_request_id  bigint NOT NULL REFERENCES forge.merge_requests (id),
    trigger           text NOT NULL CONSTRAINT autofix_attempts_trigger
        CHECK (trigger IN ('pipeline_failed', 'conflict')),
    head_sha          text NOT NULL CONSTRAINT autofix_attempts_head
        CHECK (head_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    target_sha        text NOT NULL CONSTRAINT autofix_attempts_target
        CHECK (target_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    fixer             text CONSTRAINT autofix_attempts_fixer
        CHECK (fixer ~ '^[a-z0-9][a-z0-9._-]{0,62}$'),
    state             text NOT NULL CONSTRAINT autofix_attempts_state CHECK (state IN
        ('queued', 'running', 'applied', 'no_change', 'refused', 'proposed', 'failed',
         'skipped', 'superseded')),
    reason            text CONSTRAINT autofix_attempts_reason
        CHECK (reason ~ '^[a-z][a-z0-9_]{0,63}$'),
    detail            jsonb NOT NULL DEFAULT '[]'::jsonb CONSTRAINT autofix_attempts_detail
        CHECK (jsonb_typeof(detail) = 'array' AND jsonb_array_length(detail) <= 20
            AND octet_length(detail::text) <= 8192),
    error_code        text CONSTRAINT autofix_attempts_error_code
        CHECK (error_code ~ '^FG-[A-Z]+-[0-9]{4}$'),
    input_sha         text CONSTRAINT autofix_attempts_input
        CHECK (input_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    pipeline_id       bigint REFERENCES forge.ci_pipelines (id),
    job_id            bigint REFERENCES forge.ci_jobs (id),
    commit_sha        text CONSTRAINT autofix_attempts_commit
        CHECK (commit_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    request_id        text NOT NULL CONSTRAINT autofix_attempts_request_id
        CHECK (octet_length(request_id) BETWEEN 1 AND 128),
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    finished_at       timestamptz,
    CONSTRAINT autofix_attempts_applied CHECK (state <> 'applied' OR commit_sha IS NOT NULL),
    CONSTRAINT autofix_attempts_running
        CHECK (state <> 'running' OR (fixer IS NOT NULL AND job_id IS NOT NULL)),
    CONSTRAINT autofix_attempts_finished
        CHECK ((state IN ('queued', 'running')) = (finished_at IS NULL))
);
-- One fixer per merge request, trigger, head and target (module comment).
CREATE UNIQUE INDEX autofix_attempts_once ON forge.autofix_attempts
    (merge_request_id, trigger, head_sha, target_sha, coalesce(fixer, ''));
-- The worker's work: what is queued or running.
CREATE INDEX autofix_attempts_live ON forge.autofix_attempts (id)
    WHERE state IN ('queued', 'running');
-- A merge request's latest attempt (the one line), its 24-hour count.
CREATE INDEX autofix_attempts_merge_request ON forge.autofix_attempts (merge_request_id, id DESC);
-- The version job: is this head the bot's commit (no approval carried)?
CREATE INDEX autofix_attempts_commit ON forge.autofix_attempts (commit_sha)
    WHERE commit_sha IS NOT NULL;
-- The project purge.
CREATE INDEX autofix_attempts_project ON forge.autofix_attempts (project_id);
CREATE INDEX autofix_attempts_pipeline ON forge.autofix_attempts (pipeline_id)
    WHERE pipeline_id IS NOT NULL;
CREATE INDEX autofix_attempts_job ON forge.autofix_attempts (job_id)
    WHERE job_id IS NOT NULL;

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.project_autofix TO forge_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.autofix_attempts TO forge_app;
GRANT SELECT ON forge.project_autofix TO forge_ro;
GRANT SELECT ON forge.autofix_attempts TO forge_ro;

-- The fixer's pipeline source. Each new CHECK is added NOT VALID and
-- validated before the old one goes (no constraint gap).
ALTER TABLE forge.ci_pipelines ADD CONSTRAINT ci_pipelines_source_c14
    CHECK (source IN ('push', 'merge_request', 'schedule', 'manual', 'merge_queue', 'autofix'))
    NOT VALID;
ALTER TABLE forge.ci_pipelines VALIDATE CONSTRAINT ci_pipelines_source_c14;
ALTER TABLE forge.ci_pipelines DROP CONSTRAINT ci_pipelines_source;
ALTER TABLE forge.ci_pipelines RENAME CONSTRAINT ci_pipelines_source_c14 TO ci_pipelines_source;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261221100000, "guarded_changes": []}');
