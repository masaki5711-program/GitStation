-- Plan 18 S7 (ADR-0037 (b) 5): the document fast path's batches of 32.
--
-- A queue whose verdict is in process (no `.forge-ci.yml` at its base or
-- its train) builds batches of `merge_queue.doc_batch_max` (32); a queue
-- with a CI definition keeps `merge_queue.ci_batch_max` (4). The schema
-- allows 64 so both keys can be tuned without another migration.
--
--   merge_queues.batch_limit   the queue's stored bound: 1..64. Its values and
--                              default do not change: 4 is the full mark
--                              ("not lowered", forge_core::merge_queue::
--                              FULL_MARK), 1..3 a bisection or a cut; the
--                              bound a batch is built with is the queue's
--                              kind lowered by it (batch_bound). No row is
--                              rewritten, so a binary before S7 (which
--                              refuses a stored bound above 4) still reads
--                              every queue after a rollback.
--   merge_queue_batches.size   1..64 (a rollback: Runbook merge-queue).
--
-- ADR-0030's review pattern: each new CHECK is added NOT VALID (a brief
-- ACCESS EXCLUSIVE lock, no scan) and then validated (a scan under SHARE
-- UPDATE EXCLUSIVE, writes go on) before the old one goes, so the column is
-- never without a bound; the DROP and RENAME take a brief ACCESS EXCLUSIVE
-- lock each. The whole file is one transaction, so its ACCESS EXCLUSIVE
-- locks are held to its end — the tables are small (one row per queue, per
-- batch). Widening a range never refuses an existing row.

ALTER TABLE forge.merge_queues ADD CONSTRAINT merge_queues_batch_limit_s7
    CHECK (batch_limit BETWEEN 1 AND 64) NOT VALID;
ALTER TABLE forge.merge_queues VALIDATE CONSTRAINT merge_queues_batch_limit_s7;
ALTER TABLE forge.merge_queues DROP CONSTRAINT merge_queues_batch_limit;
ALTER TABLE forge.merge_queues RENAME CONSTRAINT merge_queues_batch_limit_s7
    TO merge_queues_batch_limit;

ALTER TABLE forge.merge_queue_batches ADD CONSTRAINT merge_queue_batches_size_s7
    CHECK (size BETWEEN 1 AND 64) NOT VALID;
ALTER TABLE forge.merge_queue_batches VALIDATE CONSTRAINT merge_queue_batches_size_s7;
ALTER TABLE forge.merge_queue_batches DROP CONSTRAINT merge_queue_batches_size;
ALTER TABLE forge.merge_queue_batches RENAME CONSTRAINT merge_queue_batches_size_s7
    TO merge_queue_batches_size;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20270104100000, "guarded_changes": []}');
