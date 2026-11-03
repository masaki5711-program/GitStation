-- M3 S0 review (database): the two kind checks of 20261113100000 become
-- guards of state tables (tables.toml guarded_functions, guarded objects),
-- so doctor reports a dropped or replaced trigger or function.
--
-- Only the file that creates a guarded object may register it, the
-- migrations gate checks every file against the ledger's guard names, and
-- 20261113100000 is append-only. So the guards are re-created here under
-- new names, with the same bodies (the same constraint names in their
-- errors: service_account_profiles_kind,
-- service_account_profiles_owner_human, user_identities_human_only):
--   service_account_profiles_kinds  -> service_account_profiles_kind_guard
--   user_identities_human_only      -> user_identities_human_guard
-- (function and trigger alike). The old triggers and functions are
-- dropped; the new triggers are ENABLE ALWAYS and registered.

CREATE FUNCTION forge.service_account_profiles_kind_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM forge.users
                   WHERE id = NEW.user_id AND kind = 'service_account') THEN
        RAISE EXCEPTION USING ERRCODE = 'check_violation',
            CONSTRAINT = 'service_account_profiles_kind',
            SCHEMA = TG_TABLE_SCHEMA, TABLE = TG_TABLE_NAME,
            MESSAGE = 'a service account profile belongs to a service account';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM forge.users
                   WHERE id = NEW.owner_user_id AND kind = 'human') THEN
        RAISE EXCEPTION USING ERRCODE = 'check_violation',
            CONSTRAINT = 'service_account_profiles_owner_human',
            SCHEMA = TG_TABLE_SCHEMA, TABLE = TG_TABLE_NAME,
            MESSAGE = 'a service account is owned by a person';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.service_account_profiles_kind_guard() FROM PUBLIC;
DROP TRIGGER service_account_profiles_kinds ON forge.service_account_profiles;
DROP FUNCTION forge.service_account_profiles_kinds();
CREATE TRIGGER service_account_profiles_kind_guard
    BEFORE INSERT OR UPDATE OF user_id, owner_user_id ON forge.service_account_profiles
    FOR EACH ROW EXECUTE FUNCTION forge.service_account_profiles_kind_guard();
ALTER TABLE forge.service_account_profiles
    ENABLE ALWAYS TRIGGER service_account_profiles_kind_guard;

CREATE FUNCTION forge.user_identities_human_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM forge.users WHERE id = NEW.user_id AND kind = 'human') THEN
        RAISE EXCEPTION USING ERRCODE = 'check_violation',
            CONSTRAINT = 'user_identities_human_only',
            SCHEMA = TG_TABLE_SCHEMA, TABLE = TG_TABLE_NAME,
            MESSAGE = 'only a person''s account is linked to an identity';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.user_identities_human_guard() FROM PUBLIC;
DROP TRIGGER user_identities_human_only ON forge.user_identities;
DROP FUNCTION forge.user_identities_human_only();
CREATE TRIGGER user_identities_human_guard
    BEFORE INSERT OR UPDATE OF user_id ON forge.user_identities
    FOR EACH ROW EXECUTE FUNCTION forge.user_identities_human_guard();
ALTER TABLE forge.user_identities ENABLE ALWAYS TRIGGER user_identities_human_guard;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('function', 'forge.service_account_profiles_kind_guard()'),
    ('trigger', 'service_account_profiles_kind_guard on forge.service_account_profiles'),
    ('function', 'forge.user_identities_human_guard()'),
    ('trigger', 'user_identities_human_guard on forge.user_identities')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261113100200, "guarded_changes": ["forge.service_account_profiles_kind_guard", "forge.service_account_profiles.service_account_profiles_kind_guard", "forge.user_identities_human_guard", "forge.user_identities.user_identities_human_guard"]}');
