-- M3 X6 review (ADR-0024 Amendment X6, "X6 の独立レビュー"): what the
-- conflict scan needs to stay honest.
--
-- merge_request_conflict_scans
--   scanned_target_sha  the target tip the last finished scan read (readers
--                       compare it with the live tip: `stale`)
--   sweep_attempts      how many times the worker's sweep re-requested this
--                       row since its last full scan (capped; reset by a
--                       request that is not the sweep's)
-- merge_request_conflict_pairs
--   last_decided        the last outcome that was not `failed` (a pair that
--                       goes conflict → failed → conflict is not predicted,
--                       nor mailed, twice)

ALTER TABLE forge.merge_request_conflict_scans
    ADD COLUMN scanned_target_sha text
        CONSTRAINT merge_request_conflict_scans_target_sha
        CHECK (scanned_target_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    ADD COLUMN sweep_attempts integer NOT NULL DEFAULT 0
        CONSTRAINT merge_request_conflict_scans_sweep_attempts
        CHECK (sweep_attempts BETWEEN 0 AND 100);

ALTER TABLE forge.merge_request_conflict_pairs
    ADD COLUMN last_decided text
        CONSTRAINT merge_request_conflict_pairs_last_decided
        CHECK (last_decided IN ('no_overlap', 'clean', 'conflict'));

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261123100200, "guarded_changes": []}');
