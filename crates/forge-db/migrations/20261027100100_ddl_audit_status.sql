-- Step C2 (ADR-0018 §3 with the 2026-10-27 amendment): decide at read time
-- whether each row of the DDL log was expected. A row is expected when its
-- transaction is
-- - a bootstrap run (forge_audit.bootstrap_log has its txid), or
-- - a migration: forge.audit_log has a db.migrate row with its txid, it ran
--   as forge_migrator under application_name forge-migrate, and — for a
--   guarded object (or a child of one) — the row's details.guarded_changes
--   names it (as forge.<name>, forge.<function>, forge.<table>.<trigger>
--   or its parent table).
-- GRANT / REVOKE rows carry no object (PostgreSQL); doctor's privilege
-- checks cover them, so here they count as expected inside a migration.
-- forge doctor reports unexpected rows without an acknowledgement
-- (forge_audit.ddl_audit_ack) as FG-DB-0009 reason=unexpected_ddl.

CREATE VIEW forge.ddl_audit_status AS
SELECT d.id,
       d.occurred_at,
       d.txid,
       d.event,
       d.session_user_name,
       d.application_name,
       d.command_tag,
       d.object_type,
       d.object_identity,
       d.parent_identity,
       (b.txid IS NOT NULL
        OR (m.txid IS NOT NULL
            AND d.session_user_name = 'forge_migrator'
            AND d.application_name = 'forge-migrate'
            AND (NOT g.guarded OR l.listed))) AS expected,
       EXISTS (SELECT 1 FROM forge_audit.ddl_audit_ack AS a WHERE a.ddl_audit_id = d.id)
           AS acknowledged
FROM forge_audit.ddl_audit AS d
LEFT JOIN LATERAL (
    SELECT bl.txid FROM forge_audit.bootstrap_log AS bl WHERE bl.txid = d.txid LIMIT 1
) AS b ON true
LEFT JOIN LATERAL (
    SELECT al.txid, al.details FROM forge.audit_log AS al
    WHERE al.txid = d.txid AND al.action = 'db.migrate' LIMIT 1
) AS m ON true
CROSS JOIN LATERAL (
    SELECT EXISTS (
        SELECT 1 FROM forge_audit.guarded_objects AS go
        WHERE go.object_identity IN (d.object_identity, d.parent_identity)
    ) AS guarded
) AS g
CROSS JOIN LATERAL (
    SELECT coalesce(m.details -> 'guarded_changes', '[]'::jsonb) ?| pg_catalog.array_remove(ARRAY[
        d.object_identity,
        pg_catalog.regexp_replace(d.object_identity, '\(.*\)$', ''),
        d.parent_identity,
        CASE WHEN d.object_type = 'trigger' THEN
            pg_catalog.split_part(d.object_identity, ' on ', 2) || '.'
            || pg_catalog.split_part(d.object_identity, ' on ', 1)
        END
    ], NULL) AS listed
) AS l;

GRANT SELECT ON forge.ddl_audit_status TO forge_app, forge_ro, forge_maint;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261027100100, "guarded_changes": []}');
