-- M3 I1a review (database, security): two changes to 20261114100000.
--
-- 1. integrations_event_types_entries: integrations_event_types_count only
--    counted the elements, so a NULL, an empty or blank entry, an entry
--    with spaces or capitals, or a two-dimensional array passed. Every
--    entry now must be '*', an event type name or a 'family.*' (lowercase
--    letters and '_' separated by '.'; forge-core checks that it names the
--    integration class), the array one-dimensional. No subquery: the
--    entries are joined with spaces (a space is in no valid entry), the
--    join must split back into as many entries as the array has (no entry
--    holds a space or is empty) and match the entry pattern.
--
-- 2. forge_app may UPDATE forge.integration_bindings: removing the
--    membership a binding granted clears its granted_membership in the
--    same change (projects::remove_member / remove_member_decided), so a
--    later unbind never removes a membership a person gave again. The
--    audit trigger already covers UPDATE OF granted_membership (the
--    removal's member.removed row is on the same project).

ALTER TABLE forge.integrations
    ADD CONSTRAINT integrations_event_types_entries
    CHECK (array_ndims(event_types) = 1
        AND array_position(event_types, NULL) IS NULL
        AND cardinality(string_to_array(array_to_string(event_types, ' '), ' '))
            = cardinality(event_types)
        AND (' ' || array_to_string(event_types, ' '))
            ~ '^( (\*|[a-z_]+(\.[a-z_]+)*(\.\*)?))+$');

GRANT UPDATE ON forge.integration_bindings TO forge_app;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261114100100, "guarded_changes": ["forge.integrations", "forge.integration_bindings"]}');
