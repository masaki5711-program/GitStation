-- Plan 18 S9 (ADR-0037 (d) 4): speculative trains.
--
-- A queue may hold up to `merge_queue.speculation_depth` live batches
-- (default 1 = ADR-0030's one live batch). Batch k+1 is built on batch k's
-- train tip and tested at the same time; it lands only after k landed and
-- the target is its own base. A batch that ends without landing ends every
-- live batch behind it (`reason = predecessor_failed`).
--
--   merge_queue_batches.depth            the place in the queue's live chain:
--                                        1 = the front (built on the target
--                                        or, after its parent landed, now on
--                                        it), 2 = on the front's train, ...
--                                        (1..8: the key's range). A landing
--                                        moves the batches behind it one
--                                        place forward in its transaction.
--                                        Every existing row is 1.
--   merge_queue_batches.parent_batch_id  the batch whose train it was built
--                                        on (NULL: built on the target). Kept
--                                        after the parent landed or ended.
--
-- The one-live-batch index becomes one live batch per (queue, depth): the
-- new index is built before the old one is dropped, in this one
-- transaction, so the queue is never without a uniqueness guard. Depth 1
-- writes only depth 1, so the new index refuses exactly what the old one
-- did while the key is 1.
--
-- Rollback (a binary before S9): set `merge_queue.speculation_depth = 1`
-- and let the speculative batches end first (Runbook merge-queue); a
-- binary before S9 reads one live batch per queue and ignores both
-- columns. The old index is not recreated: it would refuse two live rows.
--
-- Locks: ADD COLUMN with a constant default and ADD CONSTRAINT ... NOT
-- VALID take a brief ACCESS EXCLUSIVE lock without a rewrite; VALIDATE
-- scans under SHARE UPDATE EXCLUSIVE; CREATE INDEX (not CONCURRENTLY: one
-- transaction) holds SHARE for its build. The table is small (one row per
-- batch).

ALTER TABLE forge.merge_queue_batches
    ADD COLUMN depth integer NOT NULL DEFAULT 1,
    ADD COLUMN parent_batch_id bigint;

ALTER TABLE forge.merge_queue_batches ADD CONSTRAINT merge_queue_batches_depth
    CHECK (depth BETWEEN 1 AND 8) NOT VALID;
ALTER TABLE forge.merge_queue_batches VALIDATE CONSTRAINT merge_queue_batches_depth;

ALTER TABLE forge.merge_queue_batches ADD CONSTRAINT merge_queue_batches_parent
    FOREIGN KEY (parent_batch_id) REFERENCES forge.merge_queue_batches (id) NOT VALID;
ALTER TABLE forge.merge_queue_batches VALIDATE CONSTRAINT merge_queue_batches_parent;

-- A batch behind the front was built on a parent.
ALTER TABLE forge.merge_queue_batches ADD CONSTRAINT merge_queue_batches_speculative
    CHECK (depth = 1 OR parent_batch_id IS NOT NULL) NOT VALID;
ALTER TABLE forge.merge_queue_batches VALIDATE CONSTRAINT merge_queue_batches_speculative;

CREATE UNIQUE INDEX merge_queue_batches_live_depth
    ON forge.merge_queue_batches (queue_id, depth)
    WHERE state IN ('testing', 'landing');
DROP INDEX forge.merge_queue_batches_one_live;

-- The foreign key's index (the project purge deletes a project's batches).
CREATE INDEX merge_queue_batches_parent_batch ON forge.merge_queue_batches (parent_batch_id)
    WHERE parent_batch_id IS NOT NULL;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20270115100000, "guarded_changes": []}');
