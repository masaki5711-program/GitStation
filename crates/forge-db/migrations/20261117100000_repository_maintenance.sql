-- M3 O3 (plan 11 §5 O3; ADR-0024 "実装で決めたこと（O3）"): the ledger of
-- the repository maintenance (`repository.maintain` job, `forge worker`).
--
-- repository_maintenance: one row per repository the maintenance ever
-- looked at. DERIVED / operational state (tables.toml kind = "derived"): the
-- job writes it every run, no person decides anything in it, and it is
-- recomputed by the next run; nothing reads it for an authorization or a
-- merge decision. Read by the scheduler (`next_due_at`), the job (the
-- previous fsck) and `forge doctor` (`repositories.maintenance`,
-- `repositories.integrity`, `repositories.refs`).
--
--   outcome          running | succeeded | skipped | failed | corrupt
--                    (`running` from the job's start until it records its end)
--   error_code       the FG code of a failed / corrupt run
--   skipped_reason   why a run did nothing (`incoming_push`, `gc_running`,
--                    `not_ready`), or which step it left out
--                    (`pack_refs_merge_lock_held`)
--   failures         consecutive failed runs (0 after any success); doctor
--                    alerts at 2
--   objects / size_pack_kib / refs
--                    the last counts (`count-objects -v`, every ref incl.
--                    hidden merge request refs)
--   last_fsck_at / last_fsck_mode / fsck_ok
--                    the last fsck; `fsck_ok = false` stops pruning until a
--                    clean fsck
--   last_full_fsck_at the last full (not connectivity-only) fsck
--   next_due_at      when the scheduler enqueues it again
--
-- forge_app writes it (the job) and deletes it in the project purge
-- (projects/purge.rs, before the repository row).

CREATE TABLE forge.repository_maintenance (
    repo_id            bigint PRIMARY KEY REFERENCES forge.repositories (id),
    project_id         bigint NOT NULL REFERENCES forge.projects (id),
    last_started_at    timestamptz,
    last_finished_at   timestamptz,
    outcome            text CONSTRAINT repository_maintenance_outcome
        CHECK (outcome IN ('running', 'succeeded', 'skipped', 'failed', 'corrupt')),
    error_code         text CONSTRAINT repository_maintenance_error_code
        CHECK (error_code ~ '^FG-[A-Z]{2,8}-[0-9]{4}$'),
    skipped_reason     text CONSTRAINT repository_maintenance_skipped_reason
        CHECK (skipped_reason ~ '^[a-z][a-z0-9_]{0,63}$'),
    failures           integer NOT NULL DEFAULT 0
        CONSTRAINT repository_maintenance_failures CHECK (failures >= 0),
    objects            bigint CONSTRAINT repository_maintenance_objects CHECK (objects >= 0),
    size_pack_kib      bigint CONSTRAINT repository_maintenance_size CHECK (size_pack_kib >= 0),
    refs               bigint CONSTRAINT repository_maintenance_refs CHECK (refs >= 0),
    last_fsck_at       timestamptz,
    last_fsck_mode     text CONSTRAINT repository_maintenance_fsck_mode
        CHECK (last_fsck_mode IN ('connectivity', 'full')),
    fsck_ok            boolean,
    last_full_fsck_at  timestamptz,
    next_due_at        timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT repository_maintenance_failed_has_code
        CHECK ((outcome IN ('failed', 'corrupt')) = (error_code IS NOT NULL)),
    CONSTRAINT repository_maintenance_fsck_shape
        CHECK ((last_fsck_at IS NULL) = (last_fsck_mode IS NULL)
            AND (last_fsck_at IS NULL) = (fsck_ok IS NULL))
);
-- The scheduler reads the due rows, oldest first.
CREATE INDEX repository_maintenance_due ON forge.repository_maintenance (next_due_at);
-- The project purge deletes a project's rows.
CREATE INDEX repository_maintenance_project ON forge.repository_maintenance (project_id);

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.repository_maintenance TO forge_app;
GRANT SELECT ON forge.repository_maintenance TO forge_ro;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261117100000, "guarded_changes": []}');
