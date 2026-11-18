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
--       differ and the stamp below says the row is its own.
--   paths_rule
--       the version of the rule the set was built for (forge-core
--       merge_paths RULE_VERSION). A set of another version is unknown:
--       the carry-forward and the readers ask for exactly their own.
--   paths_head_sha, paths_seq
--       the stamp of the check that wrote the five columns above: the
--       head it checked and its check_seq. The set and
--       computed_target_sha count only while paths_head_sha =
--       checked_head_sha and paths_seq = check_seq. A writer that does
--       not know these columns (the binary before this migration, during
--       a rolling restart of serve and worker or after a rollback)
--       stores a result with a new check_seq and leaves them as they
--       were: the stamp no longer matches, so its row is never carried
--       on a set (or shown as carried from a tip) that belongs to an
--       earlier check.
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
-- merge requests of one project and target branch — the partial unique
-- index merge_requests_one_open (project_id, source_branch, target_branch)
-- WHERE state = 'open' of 20261106100000 (the project's open rows, the
-- target branch filtered in the index), or merge_requests_open_project
-- (project_id, id) WHERE state = 'open' of 20261208163000 — then this
-- table's primary key, and compares the
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
    ADD COLUMN paths_rule integer
        CONSTRAINT merge_request_mergeability_paths_rule CHECK (paths_rule >= 1),
    ADD COLUMN paths_head_sha text
        CONSTRAINT merge_request_mergeability_paths_head CHECK (
            paths_head_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    ADD COLUMN paths_seq bigint
        CONSTRAINT merge_request_mergeability_paths_seq CHECK (paths_seq >= 1),
    ADD CONSTRAINT merge_request_mergeability_paths CHECK (
        num_nulls(head_files, head_dirs, head_deleted_dirs, paths_complete, paths_rule)
            IN (0, 5)
        AND cardinality(head_files) <= 20000
        AND cardinality(head_dirs) <= 20000
        AND cardinality(head_deleted_dirs) <= 20000
        AND array_position(head_files, NULL) IS NULL
        AND array_position(head_dirs, NULL) IS NULL
        AND array_position(head_deleted_dirs, NULL) IS NULL);

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261229100200, "guarded_changes": []}');
