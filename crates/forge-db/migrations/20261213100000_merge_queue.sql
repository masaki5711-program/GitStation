-- M4 C8 (plan 14 §4.1, §5 C8, §9 decision 5; ADR-0030): the merge queue.
--
-- DERIVED state (tables.toml kind = "derived"): forge's worker moves
-- entries and batches without a person behind each step. What a PERSON
-- does (enqueue, dequeue, pause, resume) is audited by the command in the
-- same transaction (forge_db::review::merge_queue, ChangeTx:
-- `merge_queue.enqueued` / `.dequeued` / `.paused` / `.resumed` on the
-- project). A landed entry is audited by its merge attempt's tx2
-- (`merge_request.merged`, forge.merge_request_merges.id = merge_id).
--
-- forge.merge_queues: one queue per (project, target branch), created by
-- the first enqueue.
--   paused / paused_by / paused_at   the emergency stop: no new batch and
--                                    no landing while true
--   batch_limit   the bound of the next batch (1..4): 4, halved (rounded
--                 up) after a red batch, back to 4 after a landing
--   lease_owner / lease_until        the worker working the queue now
--                 (claimed with FOR UPDATE SKIP LOCKED, released after the
--                 step; an expired lease is claimable again)
--   next_run_at   when the queue wants its next step
--
-- forge.merge_queue_batches: one train tested at a time per queue
-- (merge_queue_batches_one_live).
--   state      testing | landing | landed | failed | abandoned
--   base_sha   the target tip the train was built on
--   train_sha  the train's last commit (refs/merge-queue/<queue_id>/train;
--              the merge_queue pipeline's sha)
--   size       its entries (1..4)
--   deadline_at  testing past it is reaped (FG-CI-0019)
--
-- forge.merge_queue_entries: one merge request in a queue; at most one
-- live (queued | testing) per merge request (merge_queue_entries_one_live).
--   position   the order (a global identity: later is behind)
--   head_sha / method / message   frozen at the enqueue
--   enqueued_by / credential_kind / credential_id   who asked, with what
--              (the landing decides again for them, ADR-0030 §6)
--   isolate    migration | risk_stop | fast_forward: a batch of its own
--   batch_id / train_sha   its live batch and its commit in the train
--   merge_id   the merge attempt the landing wrote
--   reaps      times its batch passed the deadline (removed at 3)
--   reason / error_code   why it was removed
--
-- ci_pipelines gains source 'merge_queue' (the train's pipeline: ref = the
-- target branch, no merge request, never protected), deduplicated per
-- (project, ref, sha) like push / merge_request.

CREATE TABLE forge.merge_queues (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    project_id     bigint NOT NULL REFERENCES forge.projects (id),
    target_branch  text NOT NULL CONSTRAINT merge_queues_target CHECK (
        target_branch LIKE 'refs/heads/_%' AND octet_length(target_branch) <= 255),
    paused         boolean NOT NULL DEFAULT false,
    paused_by      bigint REFERENCES forge.users (id),
    paused_at      timestamptz,
    batch_limit    integer NOT NULL DEFAULT 4 CONSTRAINT merge_queues_batch_limit
        CHECK (batch_limit BETWEEN 1 AND 4),
    lease_owner    text CONSTRAINT merge_queues_lease_owner
        CHECK (octet_length(lease_owner) BETWEEN 1 AND 128),
    lease_until    timestamptz,
    next_run_at    timestamptz NOT NULL DEFAULT now(),
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT merge_queues_once UNIQUE (project_id, target_branch),
    CONSTRAINT merge_queues_lease CHECK ((lease_owner IS NULL) = (lease_until IS NULL)),
    CONSTRAINT merge_queues_pause CHECK (paused OR (paused_by IS NULL AND paused_at IS NULL))
);
-- The worker's claim: queues due now.
CREATE INDEX merge_queues_due ON forge.merge_queues (next_run_at);
CREATE INDEX merge_queues_paused_by ON forge.merge_queues (paused_by)
    WHERE paused_by IS NOT NULL;

CREATE TABLE forge.merge_queue_batches (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    queue_id     bigint NOT NULL REFERENCES forge.merge_queues (id),
    project_id   bigint NOT NULL REFERENCES forge.projects (id),
    state        text NOT NULL CONSTRAINT merge_queue_batches_state CHECK (state IN
        ('testing', 'landing', 'landed', 'failed', 'abandoned')),
    base_sha     text NOT NULL CONSTRAINT merge_queue_batches_base
        CHECK (base_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    train_sha    text NOT NULL CONSTRAINT merge_queue_batches_train
        CHECK (train_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    size         integer NOT NULL CONSTRAINT merge_queue_batches_size CHECK (size BETWEEN 1 AND 4),
    request_id   text NOT NULL CONSTRAINT merge_queue_batches_request_id
        CHECK (octet_length(request_id) BETWEEN 1 AND 128),
    reason       text CONSTRAINT merge_queue_batches_reason
        CHECK (reason ~ '^[a-z][a-z0-9_]{0,63}$'),
    error_code   text CONSTRAINT merge_queue_batches_error_code
        CHECK (error_code ~ '^FG-[A-Z]+-[0-9]{4}$'),
    deadline_at  timestamptz NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    finished_at  timestamptz,
    CONSTRAINT merge_queue_batches_finished
        CHECK ((state IN ('testing', 'landing')) = (finished_at IS NULL))
);
CREATE UNIQUE INDEX merge_queue_batches_one_live ON forge.merge_queue_batches (queue_id)
    WHERE state IN ('testing', 'landing');
CREATE INDEX merge_queue_batches_queue ON forge.merge_queue_batches (queue_id, id DESC);
-- The project purge deletes a project's rows.
CREATE INDEX merge_queue_batches_project ON forge.merge_queue_batches (project_id);

CREATE TABLE forge.merge_queue_entries (
    id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    queue_id         bigint NOT NULL REFERENCES forge.merge_queues (id),
    project_id       bigint NOT NULL REFERENCES forge.projects (id),
    merge_request_id bigint NOT NULL REFERENCES forge.merge_requests (id),
    position         bigint GENERATED ALWAYS AS IDENTITY,
    state            text NOT NULL CONSTRAINT merge_queue_entries_state CHECK (state IN
        ('queued', 'testing', 'merged', 'removed')),
    head_sha         text NOT NULL CONSTRAINT merge_queue_entries_head
        CHECK (head_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    method           text NOT NULL CONSTRAINT merge_queue_entries_method
        CHECK (method IN ('merge_commit', 'squash', 'fast_forward')),
    message          text NOT NULL CONSTRAINT merge_queue_entries_message
        CHECK (octet_length(message) BETWEEN 1 AND 65536),
    enqueued_by      bigint NOT NULL REFERENCES forge.users (id),
    credential_kind  text NOT NULL CONSTRAINT merge_queue_entries_credential_kind
        CHECK (credential_kind IN ('session', 'token')),
    credential_id    bigint NOT NULL,
    request_id       text NOT NULL CONSTRAINT merge_queue_entries_request_id
        CHECK (octet_length(request_id) BETWEEN 1 AND 128),
    isolate          text CONSTRAINT merge_queue_entries_isolate
        CHECK (isolate IN ('migration', 'risk_stop', 'fast_forward')),
    batch_id         bigint REFERENCES forge.merge_queue_batches (id),
    train_sha        text CONSTRAINT merge_queue_entries_train
        CHECK (train_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    merge_id         bigint REFERENCES forge.merge_request_merges (id),
    reaps            integer NOT NULL DEFAULT 0 CONSTRAINT merge_queue_entries_reaps
        CHECK (reaps BETWEEN 0 AND 3),
    reason           text CONSTRAINT merge_queue_entries_reason
        CHECK (reason ~ '^[a-z][a-z0-9_]{0,63}$'),
    error_code       text CONSTRAINT merge_queue_entries_error_code
        CHECK (error_code ~ '^FG-[A-Z]+-[0-9]{4}$'),
    removed_by       bigint REFERENCES forge.users (id),
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    finished_at      timestamptz,
    CONSTRAINT merge_queue_entries_queued CHECK (state <> 'queued'
        OR (batch_id IS NULL AND train_sha IS NULL AND merge_id IS NULL)),
    CONSTRAINT merge_queue_entries_testing CHECK (state <> 'testing'
        OR (batch_id IS NOT NULL AND train_sha IS NOT NULL)),
    CONSTRAINT merge_queue_entries_merged CHECK (state <> 'merged' OR merge_id IS NOT NULL),
    CONSTRAINT merge_queue_entries_finished
        CHECK ((state IN ('queued', 'testing')) = (finished_at IS NULL))
);
CREATE UNIQUE INDEX merge_queue_entries_one_live ON forge.merge_queue_entries (merge_request_id)
    WHERE state IN ('queued', 'testing');
-- The worker's batch: the live entries of a queue in order.
CREATE INDEX merge_queue_entries_live ON forge.merge_queue_entries (queue_id, position)
    WHERE state IN ('queued', 'testing');
CREATE INDEX merge_queue_entries_batch ON forge.merge_queue_entries (batch_id)
    WHERE batch_id IS NOT NULL;
CREATE INDEX merge_queue_entries_project ON forge.merge_queue_entries (project_id);
CREATE INDEX merge_queue_entries_merge_request ON forge.merge_queue_entries (merge_request_id);
CREATE INDEX merge_queue_entries_merge ON forge.merge_queue_entries (merge_id)
    WHERE merge_id IS NOT NULL;
CREATE INDEX merge_queue_entries_enqueued_by ON forge.merge_queue_entries (enqueued_by);
CREATE INDEX merge_queue_entries_removed_by ON forge.merge_queue_entries (removed_by)
    WHERE removed_by IS NOT NULL;

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.merge_queues TO forge_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.merge_queue_batches TO forge_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.merge_queue_entries TO forge_app;
GRANT SELECT ON forge.merge_queues TO forge_ro;
GRANT SELECT ON forge.merge_queue_batches TO forge_ro;
GRANT SELECT ON forge.merge_queue_entries TO forge_ro;

-- The train's pipeline (ADR-0030 §5).
ALTER TABLE forge.ci_pipelines DROP CONSTRAINT ci_pipelines_source;
ALTER TABLE forge.ci_pipelines ADD CONSTRAINT ci_pipelines_source
    CHECK (source IN ('push', 'merge_request', 'schedule', 'manual', 'merge_queue'));
DROP INDEX forge.ci_pipelines_once;
CREATE UNIQUE INDEX ci_pipelines_once ON forge.ci_pipelines
    (project_id, source, ref, sha, coalesce(merge_request_id, 0))
    WHERE source IN ('push', 'merge_request', 'merge_queue');

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261213100000, "guarded_changes": []}');
