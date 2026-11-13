-- M4 C9 (plan 14 §5 C9; plan 04 M-5 / M-6; ADR-0017 Amendment 2026-11-13):
-- auto-merge.
--
-- DERIVED state (tables.toml kind = "derived"). What a PERSON does — change
-- a project's `merge.auto`, turn an emergency stop on or off — is audited by
-- the command in the same transaction (forge_db::review::auto_merge,
-- ChangeTx: `auto_merge.mode_changed` / `.stopped` / `.resumed`, resource
-- the project or the instance). The auto-merge worker's own enqueue is
-- audited by the queue (`merge_queue.auto_enqueued`, actor system).
--
-- forge.project_auto_merge: a project's `merge.auto` and its emergency
-- stop. A project WITHOUT a row is `full` with no stop: the default of a
-- project created after this migration (2026-11-03 user decision). Every
-- project that exists now gets a row with `off` — what it effectively had
-- (nothing merged by itself) — so no existing project starts merging on
-- its own when this version is deployed; its Owner turns it on.
--   mode / mode_by / mode_at        the mode and who set it last
--   stopped_by / stopped_at         the project's emergency stop (on while
--                                   stopped_at is set)
--
-- forge.auto_merge_instance: the instance's emergency stop (one row, id 1).
--
-- forge.auto_merge_states: what auto-merge did with one open merge request
-- last (the one line on the merge request, the dev-status feed):
--   state     queued | waiting | held | stopped
--   class     the stop class (stopped only): program_breakage, data_loss,
--             leak, developer_pc, not_assessed, emergency_stop
--   reason    a stable word (risk_stop, required_check_failed, draft, ...)
--   detail    the forge/risk rule or the check behind it
--   head_sha  the head it was decided on
--   checked_at / changed_at   the last look / the last change of state
--
-- merge_queue_entries and merge_request_merges accept credential_kind
-- 'auto_merge' (credential_id = the project): the auto-merge principal's
-- entries and the attempts their landing writes. enqueued_by /
-- requested_by is then the merge request's author, on whose behalf the
-- change lands (the commit identity and `merged_by`); the principal that
-- decided is the auto-merge principal (ADR-0017 Amendment 2026-11-13).

CREATE TABLE forge.project_auto_merge (
    project_id  bigint PRIMARY KEY REFERENCES forge.projects (id),
    mode        text NOT NULL CONSTRAINT project_auto_merge_mode
        CHECK (mode IN ('off', 'reserve', 'conditional', 'full')),
    mode_by     bigint REFERENCES forge.users (id),
    mode_at     timestamptz,
    stopped_by  bigint REFERENCES forge.users (id),
    stopped_at  timestamptz,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT project_auto_merge_stop CHECK (stopped_by IS NULL OR stopped_at IS NOT NULL)
);
CREATE INDEX project_auto_merge_mode_by ON forge.project_auto_merge (mode_by)
    WHERE mode_by IS NOT NULL;
CREATE INDEX project_auto_merge_stopped_by ON forge.project_auto_merge (stopped_by)
    WHERE stopped_by IS NOT NULL;

INSERT INTO forge.project_auto_merge (project_id, mode)
SELECT id, 'off' FROM forge.projects;

CREATE TABLE forge.auto_merge_instance (
    id          smallint PRIMARY KEY CONSTRAINT auto_merge_instance_one CHECK (id = 1),
    stopped_by  bigint REFERENCES forge.users (id),
    stopped_at  timestamptz,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT auto_merge_instance_stop CHECK (stopped_by IS NULL OR stopped_at IS NOT NULL)
);
INSERT INTO forge.auto_merge_instance (id) VALUES (1);

CREATE TABLE forge.auto_merge_states (
    merge_request_id  bigint PRIMARY KEY REFERENCES forge.merge_requests (id),
    project_id        bigint NOT NULL REFERENCES forge.projects (id),
    head_sha          text NOT NULL CONSTRAINT auto_merge_states_head
        CHECK (head_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    state             text NOT NULL CONSTRAINT auto_merge_states_state
        CHECK (state IN ('queued', 'waiting', 'held', 'stopped')),
    class             text CONSTRAINT auto_merge_states_class CHECK (class IN
        ('program_breakage', 'data_loss', 'leak', 'developer_pc', 'not_assessed',
         'emergency_stop')),
    reason            text NOT NULL CONSTRAINT auto_merge_states_reason
        CHECK (reason ~ '^[a-z][a-z0-9_]{0,63}$'),
    detail            text CONSTRAINT auto_merge_states_detail
        CHECK (octet_length(detail) BETWEEN 1 AND 255),
    error_code        text CONSTRAINT auto_merge_states_error_code
        CHECK (error_code ~ '^FG-[A-Z]+-[0-9]{4}$'),
    request_id        text NOT NULL CONSTRAINT auto_merge_states_request_id
        CHECK (octet_length(request_id) BETWEEN 1 AND 128),
    checked_at        timestamptz NOT NULL DEFAULT now(),
    changed_at        timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT auto_merge_states_stopped CHECK ((state = 'stopped') = (class IS NOT NULL))
);
-- The worker's round robin and the project purge.
CREATE INDEX auto_merge_states_checked ON forge.auto_merge_states (checked_at);
CREATE INDEX auto_merge_states_project ON forge.auto_merge_states (project_id);

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.project_auto_merge TO forge_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.auto_merge_instance TO forge_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.auto_merge_states TO forge_app;
GRANT SELECT ON forge.project_auto_merge TO forge_ro;
GRANT SELECT ON forge.auto_merge_instance TO forge_ro;
GRANT SELECT ON forge.auto_merge_states TO forge_ro;

-- The auto-merge principal's credential kind. Review: each new CHECK is
-- added NOT VALID and validated before the old one goes (no constraint gap,
-- no ACCESS EXCLUSIVE scan beyond the validation).
ALTER TABLE forge.merge_queue_entries ADD CONSTRAINT merge_queue_entries_credential_kind_c9
    CHECK (credential_kind IN ('session', 'token', 'auto_merge')) NOT VALID;
ALTER TABLE forge.merge_queue_entries VALIDATE CONSTRAINT merge_queue_entries_credential_kind_c9;
ALTER TABLE forge.merge_queue_entries DROP CONSTRAINT merge_queue_entries_credential_kind;
ALTER TABLE forge.merge_queue_entries RENAME CONSTRAINT merge_queue_entries_credential_kind_c9
    TO merge_queue_entries_credential_kind;

ALTER TABLE forge.merge_request_merges ADD CONSTRAINT merge_request_merges_credential_kind_c9
    CHECK (credential_kind IN ('session', 'token', 'auto_merge')) NOT VALID;
ALTER TABLE forge.merge_request_merges VALIDATE CONSTRAINT merge_request_merges_credential_kind_c9;
ALTER TABLE forge.merge_request_merges DROP CONSTRAINT merge_request_merges_credential_kind;
ALTER TABLE forge.merge_request_merges RENAME CONSTRAINT merge_request_merges_credential_kind_c9
    TO merge_request_merges_credential_kind;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261215100000, "guarded_changes": []}');
