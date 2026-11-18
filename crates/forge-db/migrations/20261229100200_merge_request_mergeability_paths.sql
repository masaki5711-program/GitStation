-- Plan 18 S4 (ADR-0037 (c)): path-aware mergeability. A landing on a target
-- branch re-checks only the open merge requests whose changed paths may
-- interact with what landed; the result of the others is carried forward
-- to the new target tip in one statement (forge-db review::refresh).
--
--   head_files, head_dirs, head_deleted_dirs
--       the changed-path set of the checked head against its merge base
--       with the target the check computed (forge-core merge_paths: the
--       keys of the changed files, of every ancestor directory of them,
--       and of every ancestor directory of what the head deleted). Written
--       by the check that wrote the result, in the same UPDATE, so the set
--       always belongs to checked_head_sha.
--   paths_complete
--       true: the three lists are everything. false: the set is incomplete
--       (more than review.paths_max, a path that is not UTF-8, a symbolic
--       link or submodule, a global path such as .gitattributes) — the
--       lists are empty and the merge request is always checked again.
--   computed_target_sha
--       the target tip the stored result was really computed against.
--       checked_target_sha moves on when the result is carried forward;
--       this column does not. A row is "carried" exactly when the two
--       differ.
--
-- Additive only, every column nullable without a default: NULL means
-- "unknown" and is always checked again, so the rows that exist behave as
-- before until their next check writes a set. Derived state like the rest
-- of the table (tables.toml kind = "derived"); added instead of editing
-- 20261108100000 (append-only, ADR-0014).
--
-- Locking: ALTER TABLE takes ACCESS EXCLUSIVE on
-- forge.merge_request_mergeability. ADD COLUMN without a default is a
-- catalog change (no table rewrite). The CHECK constraints are verified by
-- one scan of the table under that lock: one narrow row per merge request,
-- every new value NULL (a NULL passes at once) — milliseconds; the lock is
-- released at the migration's commit.
--
-- No index: the carry-forward statement reaches its rows through the open
-- merge requests of one project and target branch
-- (forge.merge_requests, then this table's primary key) and compares the
-- arrays of each candidate row with `&&` against the bound landed lists
-- (at most a few hundred keys: the payload's landed block is bounded).
-- A GIN index on the arrays would only help if the statement searched by
-- path across projects, and would have to be maintained on every check.

ALTER TABLE forge.merge_request_mergeability
    ADD COLUMN head_files text[],
    ADD COLUMN head_dirs text[],
    ADD COLUMN head_deleted_dirs text[],
    ADD COLUMN paths_complete boolean,
    ADD COLUMN computed_target_sha text
        CONSTRAINT merge_request_mergeability_computed_target CHECK (
            computed_target_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    ADD CONSTRAINT merge_request_mergeability_paths CHECK (
        num_nulls(head_files, head_dirs, head_deleted_dirs, paths_complete) IN (0, 4)
        AND cardinality(head_files) <= 20000
        AND cardinality(head_dirs) <= 20000
        AND cardinality(head_deleted_dirs) <= 20000
        AND array_position(head_files, NULL) IS NULL
        AND array_position(head_dirs, NULL) IS NULL
        AND array_position(head_deleted_dirs, NULL) IS NULL);

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261229100200, "guarded_changes": []}');
