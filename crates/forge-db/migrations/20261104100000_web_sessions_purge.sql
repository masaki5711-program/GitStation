-- M2 MR6 (the MR3 handoff's deferred cleanup, ADR-0021 §5): forge_app may
-- DELETE from forge.web_sessions, for the purge only. The `forge worker`
-- task `session_purge` (forge_db::sessions::purge_expired) deletes rows
-- 30 days past their revocation or their absolute expiry, a bounded batch
-- per change, with one `session.purged` audit row per owner (the DELETE
-- fires audited_change_present like every change of the table).
--
-- A guard narrows the grant (for every role, ENABLE ALWAYS; XF001,
-- FG-DB-0009 reason=rejected): only a row that was revoked, or whose
-- absolute lifetime ended, more than 30 days ago (PURGE_AFTER_DAYS of
-- sessions::purge). No live or recently ended session can be deleted, so
-- no session's history goes before the audit log keeps its trail. doctor
-- compares the trigger and the function body (bootstrap/schema.rs).

CREATE FUNCTION forge.web_sessions_purge_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    -- coalesce: a session never revoked (revoked_at NULL) is judged by its
    -- absolute expiry alone (NULL would make the whole test NULL: no raise).
    IF NOT (coalesce(OLD.revoked_at < pg_catalog.now() - interval '30 days', false)
            OR OLD.absolute_expires_at < pg_catalog.now() - interval '30 days') THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a session on forge.web_sessions is deleted only 30 days after it ended';
    END IF;
    RETURN OLD;
END
$$;
REVOKE ALL ON FUNCTION forge.web_sessions_purge_guard() FROM PUBLIC;
CREATE TRIGGER web_sessions_purge_only BEFORE DELETE ON forge.web_sessions
    FOR EACH ROW EXECUTE FUNCTION forge.web_sessions_purge_guard();
ALTER TABLE forge.web_sessions ENABLE ALWAYS TRIGGER web_sessions_purge_only;

GRANT DELETE ON forge.web_sessions TO forge_app;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('function', 'forge.web_sessions_purge_guard()'),
    ('trigger', 'web_sessions_purge_only on forge.web_sessions')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261104100000, "guarded_changes": ["forge.web_sessions_purge_guard", "forge.web_sessions.web_sessions_purge_only", "forge.web_sessions"]}');
