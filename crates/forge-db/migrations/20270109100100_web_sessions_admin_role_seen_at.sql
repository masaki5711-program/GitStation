-- ADR-0021 amendment 2026-11-24 (third review MEDIUM): when the session
-- last saw the identity provider's admin role (`auth.oidc.admin_role`) in a
-- verified token. A session counts an administrator flag the role granted
-- (`users.admin_source = 'oidc'`) only while this is recent
-- (`sessions::ADMIN_CONFIRM_SECS`). It is set at the sign-in and at a
-- refresh check only when `auth.oidc.admin_role` is configured and the
-- token holds that role; with the setting off it never moves, so such a
-- flag stops counting. Separate from access_role_seen_at, which confirms
-- the access role (ADR-0021 §7) for every session.
--
-- Unaudited, like the other columns of the refresh check: the audit
-- trigger is re-created with the column in its unaudited list (doctor
-- compares the trigger with tables.toml). forge_app may update it;
-- forge_ro may read it (a time, no secret). NULL for every existing row: no
-- role was confirmed for them yet (an existing IdP-granted flag counts from
-- the next sign-in or refresh check that sees the role).

ALTER TABLE forge.web_sessions ADD COLUMN admin_role_seen_at timestamptz;

DROP TRIGGER audited_change_present ON forge.web_sessions;
CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, public_id, secret_sha256, user_id, created_at,
        absolute_expires_at, revoked_at, revoked_reason, oidc_issuer, oidc_sid, oidc_sub
    ON forge.web_sessions
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('user', 'user_id',
        'last_seen_at,idle_expires_at,refresh_token_enc,id_token_enc,refresh_checked_at,access_role_seen_at,refresh_without_id_token,admin_role_seen_at',
        '');
ALTER TABLE forge.web_sessions ENABLE ALWAYS TRIGGER audited_change_present;

GRANT UPDATE (admin_role_seen_at) ON forge.web_sessions TO forge_app;
GRANT SELECT (admin_role_seen_at) ON forge.web_sessions TO forge_ro;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20270109100100, "guarded_changes": ["forge.web_sessions.audited_change_present"]}');
