-- M3 X6 (plan 04 C-2, ADR-0024 Amendment X6): predicted conflicts between
-- open merge requests on the same target branch (drafts included).
--
-- Both tables are DERIVED state (tables.toml kind = "derived"): recomputed
-- from git at any time, never a decision of a person, so a change needs no
-- audit row. The event that matters (merge_request.conflict_predicted,
-- written when a pair ENTERS "conflict") carries its own audit rows in the
-- same transaction.
--
-- 1. merge_request_conflict_scans: one row per merge request that ever had
--    a scan requested (the version job after a new head, a retarget, a
--    reopen).
--      status       pending | scanning | scanned | incomplete | failed
--                   (`incomplete`: scanned, but some pairs were not tried —
--                   a cap, the time budget, or a git failure on a pair)
--      reason       why (incomplete: peers_capped, pairs_capped,
--                   budget_exhausted, pair_failed; failed: no_version,
--                   target_missing, unrelated_histories, git_timeout,
--                   git_busy, git_failed, job_failed, job_stale)
--      scan_seq     bumped by every request; a job writes only while its
--                   own seq is the latest (jobs are deduplicated per seq)
--      scanned_head_sha / scanned_target_branch / scanned_at
--                   what the last finished scan looked at
--      peers / pairs_tried / pairs_skipped
--                   how many other open merge requests were considered,
--                   how many trial merges ran, how many candidates were
--                   left out (the caps and the budget)
--
-- 2. merge_request_conflict_pairs: one row per unordered pair of merge
--    requests (low_mr_id < high_mr_id) a scan ever looked at, with the
--    heads it looked at. Readers trust a row only while both heads are the
--    merge requests' current heads, both are open and target
--    `target_branch`.
--      outcome      no_overlap | clean | conflict | failed
--      reason       failed: unrelated_histories, git_timeout, git_busy,
--                   git_failed
--      conflicts    the conflicted paths both change (capped by the job),
--                   conflicts_truncated when more exist
--      prediction_seq
--                   how often the pair entered "conflict" (the event's
--                   dedup key)
--
-- forge_app writes them (the commands, the job, the sweep) and deletes them
-- in the project purge (projects/purge.rs).

CREATE TABLE forge.merge_request_conflict_scans (
    merge_request_id       bigint PRIMARY KEY REFERENCES forge.merge_requests (id),
    project_id             bigint NOT NULL REFERENCES forge.projects (id),
    status                 text NOT NULL DEFAULT 'pending'
        CONSTRAINT merge_request_conflict_scans_status CHECK (status IN
            ('pending', 'scanning', 'scanned', 'incomplete', 'failed')),
    reason                 text CONSTRAINT merge_request_conflict_scans_reason
        CHECK (reason ~ '^[a-z][a-z0-9_]{0,63}$'),
    scan_seq               bigint NOT NULL DEFAULT 1
        CONSTRAINT merge_request_conflict_scans_seq CHECK (scan_seq > 0),
    scanned_head_sha       text CONSTRAINT merge_request_conflict_scans_head
        CHECK (scanned_head_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    scanned_target_branch  text COLLATE "C" CONSTRAINT merge_request_conflict_scans_target
        CHECK (scanned_target_branch ~ '^refs/heads/.'
            AND octet_length(scanned_target_branch) <= 1024),
    scanned_at             timestamptz,
    peers                  integer NOT NULL DEFAULT 0
        CONSTRAINT merge_request_conflict_scans_peers CHECK (peers >= 0),
    pairs_tried            integer NOT NULL DEFAULT 0
        CONSTRAINT merge_request_conflict_scans_tried CHECK (pairs_tried >= 0),
    pairs_skipped          integer NOT NULL DEFAULT 0
        CONSTRAINT merge_request_conflict_scans_skipped CHECK (pairs_skipped >= 0),
    updated_at             timestamptz NOT NULL DEFAULT now()
);
-- The project purge deletes a project's rows.
CREATE INDEX merge_request_conflict_scans_project
    ON forge.merge_request_conflict_scans (project_id);

CREATE TABLE forge.merge_request_conflict_pairs (
    low_mr_id            bigint NOT NULL REFERENCES forge.merge_requests (id),
    high_mr_id           bigint NOT NULL REFERENCES forge.merge_requests (id),
    project_id           bigint NOT NULL REFERENCES forge.projects (id),
    low_head_sha         text NOT NULL CONSTRAINT merge_request_conflict_pairs_low_head
        CHECK (low_head_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    high_head_sha        text NOT NULL CONSTRAINT merge_request_conflict_pairs_high_head
        CHECK (high_head_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    target_branch        text COLLATE "C" NOT NULL CONSTRAINT merge_request_conflict_pairs_target
        CHECK (target_branch ~ '^refs/heads/.' AND octet_length(target_branch) <= 1024),
    outcome              text NOT NULL
        CONSTRAINT merge_request_conflict_pairs_outcome CHECK (outcome IN
            ('no_overlap', 'clean', 'conflict', 'failed')),
    reason               text CONSTRAINT merge_request_conflict_pairs_reason
        CHECK (reason ~ '^[a-z][a-z0-9_]{0,63}$'),
    conflicts            text[] NOT NULL DEFAULT '{}'
        CONSTRAINT merge_request_conflict_pairs_conflicts CHECK (
            cardinality(conflicts) <= 1000
            AND octet_length(array_to_string(conflicts, '/')) <= 65536),
    conflicts_truncated  boolean NOT NULL DEFAULT false,
    prediction_seq       bigint NOT NULL DEFAULT 0
        CONSTRAINT merge_request_conflict_pairs_seq CHECK (prediction_seq >= 0),
    checked_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT merge_request_conflict_pairs_pk PRIMARY KEY (low_mr_id, high_mr_id),
    CONSTRAINT merge_request_conflict_pairs_ordered CHECK (low_mr_id < high_mr_id)
);
-- A merge request's pairs where it is the higher id (the primary key
-- serves the lower one); the project purge.
CREATE INDEX merge_request_conflict_pairs_high
    ON forge.merge_request_conflict_pairs (high_mr_id);
CREATE INDEX merge_request_conflict_pairs_project
    ON forge.merge_request_conflict_pairs (project_id);

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.merge_request_conflict_scans TO forge_app;
GRANT SELECT ON forge.merge_request_conflict_scans TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.merge_request_conflict_pairs TO forge_app;
GRANT SELECT ON forge.merge_request_conflict_pairs TO forge_ro;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261123100000, "guarded_changes": []}');
