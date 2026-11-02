-- M2 MR5 (ADR-0021 §7, MR1 handoff §3 follow-up): the refresh check counts
-- the refreshes in a row whose answer carried no ID token. Without an ID
-- token the access role cannot be confirmed, so the check keeps the last
-- confirmed decision only for a bounded time and for at most 3 such
-- refreshes in a row (then the session is revoked, reason idp_revoked:
-- fail closed). A refresh with an ID token resets the count to 0.
--
-- Unaudited, like the other columns of the refresh check (refresh_token_enc,
-- id_token_enc, refresh_checked_at, access_role_seen_at): the audit trigger
-- is re-created with the column in its unaudited list (doctor compares the
-- trigger with tables.toml). forge_app may update it; forge_ro may read it
-- (a count, no secret).

ALTER TABLE forge.web_sessions
    ADD COLUMN refresh_without_id_token smallint NOT NULL DEFAULT 0,
    ADD CONSTRAINT web_sessions_refresh_without_id_token_range
        CHECK (refresh_without_id_token BETWEEN 0 AND 1000);

DROP TRIGGER audited_change_present ON forge.web_sessions;
CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, public_id, secret_sha256, user_id, created_at,
        absolute_expires_at, revoked_at, revoked_reason, oidc_issuer, oidc_sid, oidc_sub
    ON forge.web_sessions
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('user', 'user_id',
        'last_seen_at,idle_expires_at,refresh_token_enc,id_token_enc,refresh_checked_at,access_role_seen_at,refresh_without_id_token',
        '');
ALTER TABLE forge.web_sessions ENABLE ALWAYS TRIGGER audited_change_present;

GRANT UPDATE (refresh_without_id_token) ON forge.web_sessions TO forge_app;
GRANT SELECT (refresh_without_id_token) ON forge.web_sessions TO forge_ro;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261104100000, "guarded_changes": ["forge.web_sessions.audited_change_present"]}');
