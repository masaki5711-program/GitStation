-- M3 C1 review fix 8 (LOW): an annotation path with a drive letter
-- (`C:/…`, `c:x`) is no path inside a repository. forge-checks refuses it
-- (FG-REVIEW-0020 annotation_path_invalid); the CHECK, the backstop of the
-- same rule, now refuses it too. The constraint keeps its name
-- (check_annotations_path) and every earlier clause; the table is small
-- (C1 ships it), so it is replaced at once.

ALTER TABLE forge.check_annotations DROP CONSTRAINT check_annotations_path;
ALTER TABLE forge.check_annotations ADD CONSTRAINT check_annotations_path
    CHECK (octet_length(path) BETWEEN 1 AND 4096
        AND path !~ '^/'
        AND path !~ '^[A-Za-z]:'
        AND path !~ '(^|/)\.\.(/|$)'
        AND path !~ '[\\[:cntrl:]]');

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261115100300, "guarded_changes": []}');
