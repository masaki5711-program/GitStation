-- M2 MR3 review (ADR-0021 §5): two backstops on forge.web_sessions, added
-- after 20261103100000 (append-only, ADR-0014).
--
-- 1. An IdP session id only exists with its issuer: oidc_sid without
--    oidc_issuer would make the back-channel logout (`oidc_issuer = iss AND
--    oidc_sid = sid`, ADR-0021 §8) unable to find the row.
-- 2. A revocation is final: once revoked_at is set, neither revoked_at nor
--    revoked_reason changes again (a revoked session never comes back, and
--    its recorded reason stays the one audited). A guard (XF001, FG-DB-0009
--    reason=rejected), enabled ALWAYS so it holds for every role; doctor
--    compares the trigger and the function body (bootstrap/schema.rs).

ALTER TABLE forge.web_sessions ADD CONSTRAINT web_sessions_oidc_sid_needs_issuer
    CHECK (oidc_sid IS NULL OR oidc_issuer IS NOT NULL);

CREATE FUNCTION forge.web_sessions_revocation_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF OLD.revoked_at IS NOT NULL
       AND (NEW.revoked_at IS DISTINCT FROM OLD.revoked_at
            OR NEW.revoked_reason IS DISTINCT FROM OLD.revoked_reason) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a revoked session on forge.web_sessions stays revoked with its reason';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.web_sessions_revocation_guard() FROM PUBLIC;
CREATE TRIGGER web_sessions_revocation_final BEFORE UPDATE ON forge.web_sessions
    FOR EACH ROW EXECUTE FUNCTION forge.web_sessions_revocation_guard();
ALTER TABLE forge.web_sessions ENABLE ALWAYS TRIGGER web_sessions_revocation_final;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('function', 'forge.web_sessions_revocation_guard()'),
    ('trigger', 'web_sessions_revocation_final on forge.web_sessions')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261103100100, "guarded_changes": ["forge.web_sessions_revocation_guard", "forge.web_sessions.web_sessions_revocation_final"]}');
