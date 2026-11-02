-- M3 R3 review (database-reviewer): a discussion's project and merge
-- request, and a note's discussion, merge request and project, were set by
-- the application alone (20261109100000): nothing in the schema stopped a
-- thread recorded under one project while its merge request belongs to
-- another (an audit row on the wrong project, a purge that misses it).
-- Composite foreign keys now tie them together:
--
--   merge_request_discussions (merge_request_id, project_id)
--     → merge_requests (id, project_id)
--   merge_request_notes (discussion_id, merge_request_id, project_id)
--     → merge_request_discussions (id, merge_request_id, project_id)
--
-- The referenced pairs get the UNIQUE constraints a foreign key needs (the
-- ids are already primary keys, so the constraints add no new rule). The
-- tables are new in this release, so the checks run on few rows. Appended
-- instead of editing 20261109100000 (append-only, ADR-0014).

ALTER TABLE forge.merge_requests
    ADD CONSTRAINT merge_requests_id_project UNIQUE (id, project_id);

ALTER TABLE forge.merge_request_discussions
    ADD CONSTRAINT merge_request_discussions_scope UNIQUE (id, merge_request_id, project_id),
    ADD CONSTRAINT merge_request_discussions_merge_request_project
        FOREIGN KEY (merge_request_id, project_id)
        REFERENCES forge.merge_requests (id, project_id);

ALTER TABLE forge.merge_request_notes
    ADD CONSTRAINT merge_request_notes_discussion_scope
        FOREIGN KEY (discussion_id, merge_request_id, project_id)
        REFERENCES forge.merge_request_discussions (id, merge_request_id, project_id);

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261109100100, "guarded_changes": []}');
