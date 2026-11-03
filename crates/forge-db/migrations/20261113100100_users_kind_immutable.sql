-- M3 S0 review (database, MEDIUM): a user's kind never changes.
--
-- service_account_profiles_kinds and user_identities_human_only
-- (20261113100000) check users.kind when their own row is written; a later
-- UPDATE of forge.users.kind (forge_app holds UPDATE on the table) would
-- leave a profile on a person or an identity on a service account without
-- either trigger seeing it. forge-db never changes users.kind, so the
-- column is made immutable: users_kind_immutable refuses any change of it
-- (constraint name users_kind_immutable, SQLSTATE 23514), for every role.
--
-- users_kind_immutable is a guard of a state table (tables.toml
-- guarded_functions, a guarded object): a dropped or replaced trigger or
-- function is reported by `forge doctor`. Doctor also compares the two
-- kind triggers of 20261113100000 (STATE_GUARD_TRIGGERS, GUARD_FUNCTIONS);
-- they are not registered as guarded objects, which only the file that
-- creates an object may do (the migrations gate), and that file is
-- applied and append-only.
--
-- Pre-deploy check for 20261113100000 (its users CHECKs validate existing
-- rows; the migration fails as a whole if one breaks them). Run on the
-- target before `forge db migrate`; both counts must be 0:
--   SELECT count(*) FILTER (WHERE email IS NOT NULL OR employee_number IS NOT NULL
--          OR external_id IS NOT NULL) AS with_personal_data,
--          count(*) FILTER (WHERE is_admin) AS administrators
--   FROM forge.users WHERE kind = 'service_account';

CREATE FUNCTION forge.users_kind_immutable() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NEW.kind IS DISTINCT FROM OLD.kind THEN
        RAISE EXCEPTION USING ERRCODE = 'check_violation',
            CONSTRAINT = 'users_kind_immutable',
            SCHEMA = TG_TABLE_SCHEMA, TABLE = TG_TABLE_NAME,
            MESSAGE = 'the kind of a user on forge.users never changes';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.users_kind_immutable() FROM PUBLIC;
CREATE TRIGGER users_kind_immutable BEFORE UPDATE OF kind ON forge.users
    FOR EACH ROW EXECUTE FUNCTION forge.users_kind_immutable();
ALTER TABLE forge.users ENABLE ALWAYS TRIGGER users_kind_immutable;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('function', 'forge.users_kind_immutable()'),
    ('trigger', 'users_kind_immutable on forge.users')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261113100100, "guarded_changes": ["forge.users_kind_immutable", "forge.users.users_kind_immutable"]}');
