-- M2 MR6 review (database, MEDIUM): the session purge
-- (forge_db::sessions::purge_expired) selects
--   revoked_at < now() - 30 days OR absolute_expires_at < now() - 30 days
-- in id order. Without an index on either side that is a scan of every
-- session row each run; with one per side the planner can combine them
-- (BitmapOr). Partial on revoked_at: most rows are never revoked early.
-- Indexes only (append-only, ADR-0014); no privilege or trigger changes.

CREATE INDEX web_sessions_purge_revoked ON forge.web_sessions (revoked_at)
    WHERE revoked_at IS NOT NULL;
CREATE INDEX web_sessions_purge_expiry ON forge.web_sessions (absolute_expires_at);

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261105100100, "guarded_changes": ["forge.web_sessions"]}');
