-- M3 R2 re-review (ADR-0024 §3): a decided mergeability (can / cannot, or a
-- deterministic check_failed) whose checked pair is no longer the live
-- (head, target tip) — its push event dead-lettered, or the target push was
-- past the deferral bound — was shown as stale for ever: nothing asked
-- again. The worker's sweep now visits decided rows, the least recently
-- verified first, compares the checked pair with the live one (git) and
-- either re-requests the check or records that the pair is still live.
--
-- confirmed_at: when the sweep last saw the checked pair still live (NULL:
-- never; the sweep orders by COALESCE(confirmed_at, checked_at)). Derived
-- state like the rest of the table (tables.toml kind = "derived"); added
-- instead of editing 20261108100000 (append-only, ADR-0014).

ALTER TABLE forge.merge_request_mergeability ADD COLUMN confirmed_at timestamptz;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261108100100, "guarded_changes": []}');
