-- Plan 18 S7 (ADR-0037 (b) 5): the document fast path's batches of 32.
--
-- A queue whose verdict is in process (no `.forge-ci.yml` at its base or
-- its train) builds batches of `merge_queue.doc_batch_max` (32); a queue
-- with a CI definition keeps `merge_queue.ci_batch_max` (4). The schema
-- allows 64 so both keys can be tuned without another migration.
--
--   merge_queues.batch_limit   the queue's stored bound: 1..64. 64 (the
--                              schema's bound, forge_core::merge_queue::
--                              MAX_BATCH) until a red batch bisects it or a
--                              merge result cuts it; back to 64 after a
--                              landing. The bound a batch is built with is
--                              the smaller of it and the queue's kind
--                              (forge_core::merge_queue::batch_bound).
--   merge_queue_batches.size   1..64.
--
-- ADR-0030's review pattern: each new CHECK is added NOT VALID and
-- validated (no ACCESS EXCLUSIVE scan beyond the validation's SHARE UPDATE
-- EXCLUSIVE) before the old one goes, so the column is never without a
-- bound (the whole file is one transaction anyway). Widening a range never
-- refuses an existing row.

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

-- A new queue starts at the schema's bound; its kind bounds the batch.
ALTER TABLE forge.merge_queues ALTER COLUMN batch_limit SET DEFAULT 64;
-- A queue at the old full bound (4: not lowered by a bisection) is at the
-- new full bound. A lowered one (1..3) stays lowered until its next
-- landing resets it, as before.
UPDATE forge.merge_queues SET batch_limit = 64 WHERE batch_limit = 4;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20270104100000, "guarded_changes": []}');
