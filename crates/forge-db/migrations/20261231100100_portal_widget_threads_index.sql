-- Portal widgets P1 (ADR-0036, docs/portal/widget-design.md §3.2): the
-- `my-reviews?kind=threads` widget lists the open merge requests where an
-- unresolved thread the person started has a newer note by somebody else
-- (forge-db review::user_lists::WidgetList::Threads). The statement starts
-- from the person's unresolved threads; merge_request_discussions had no
-- index on started_by (only merge_request_id and project_id), so every read
-- walked the whole table. The portal answers within 3 s for every open
-- home page, refreshing once a minute.
--
-- - merge_request_discussions_started_open (started_by, merge_request_id)
--   WHERE NOT resolved: the person's unresolved threads, with the merge
--   request the widget joins. Partial: resolved threads (most of them over
--   time) stay out.
--
-- No column, constraint or trigger changes; nothing guarded changes (the
-- db.migrate row lists none, as 20261229100300).
--
-- Locking: a plain CREATE INDEX takes a SHARE lock on the table for its
-- build: new threads and resolutions wait until it finishes, reads
-- continue. The table holds a row per thread (thousands to tens of
-- thousands), so the build is short. CREATE INDEX CONCURRENTLY is not
-- possible here: sqlx runs each migration in a transaction. On a large
-- installation run it in a quiet window (runbook db-migration).

CREATE INDEX merge_request_discussions_started_open
    ON forge.merge_request_discussions (started_by, merge_request_id)
    WHERE NOT resolved;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261231100100, "guarded_changes": []}');
