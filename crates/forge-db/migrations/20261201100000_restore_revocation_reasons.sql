-- M3 O1 (ADR-0019 Amendment plan 11 P1, runbook backup-restore): `forge
-- restore finish` revokes every credential issued before the restore, so a
-- token, a session or a webhook secret revoked after the backup is not
-- brought back by the restored database. Each revocation names its cause:
--
--   forge.tokens.revoked_reason        + 'restore' (personal and service
--                                         account tokens alike)
--   forge.web_sessions.revoked_reason  + 'restore'
--                                         (forge_core::SessionRevokeReason)
--   forge.webhooks.disabled_reason     + 'restore'
--                                         (forge_core::webhook::DisableReason:
--                                         rotate the secret, then enable)
--
-- No grant changes: forge_app already updates these columns. The CHECKs are
-- not guarded objects (tables.toml guarded = false for all three tables).
-- Locking: DROP / ADD CONSTRAINT takes an ACCESS EXCLUSIVE lock and scans
-- each table once to validate the new CHECK (small tables).

ALTER TABLE forge.tokens DROP CONSTRAINT tokens_revoked_reason;
ALTER TABLE forge.tokens ADD CONSTRAINT tokens_revoked_reason CHECK (
    revoked_reason IS NULL OR revoked_reason IN ('revoked', 'user_disabled', 'restore'));

ALTER TABLE forge.web_sessions DROP CONSTRAINT web_sessions_revoked_reason;
ALTER TABLE forge.web_sessions ADD CONSTRAINT web_sessions_revoked_reason CHECK (
    revoked_reason IS NULL
    OR revoked_reason IN ('logout', 'backchannel', 'user_disabled', 'admin', 'idp_revoked',
        'restore'));

ALTER TABLE forge.webhooks DROP CONSTRAINT webhooks_disabled_reason_known;
ALTER TABLE forge.webhooks ADD CONSTRAINT webhooks_disabled_reason_known CHECK (
    disabled_reason IN ('manual', 'failures', 'failing_24h', 'restore'));

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261201100000, "guarded_changes": []}');
