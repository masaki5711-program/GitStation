-- M3 S0 (plan 11 §5 S0, ADR-0019 Addendum S0, ADR-0024 §8.4): service
-- accounts as accounts of their own. A service account is a forge.users row
-- with kind = 'service_account' (it has a personal namespace like every
-- user, a membership in each project it works on, and tokens with scopes),
-- plus one profile row naming the person who answers for it.
--
-- 1. service_account_profiles: one row per service account. user_id is
--    the account (kind must be service_account), owner_user_id the human
--    who answers for it (offboarding hands it on; doctor
--    `service_accounts.ownerless` reports a disabled owner), purpose why it
--    exists (one account per purpose, plan 11 Q4), created_by the
--    administrator who created it. The kinds are checked by
--    service_account_profiles_kinds (constraint names
--    service_account_profiles_kind / service_account_profiles_owner_human;
--    forge-db never changes users.kind, so checking on insert and on a
--    changed user_id / owner_user_id suffices). Audited on the account
--    (resource 'user', id user_id): service_account.created /
--    service_account.updated. No DELETE: a service account is disabled,
--    never removed.
--
-- 2. forge.users: a service account holds no personal data and no
--    administrator rights (users_service_account_no_personal_data,
--    users_service_account_not_admin): no e-mail, employee number or
--    external id (it never signs in through the IdP and is never matched
--    to a person), never is_admin (its tokens can never carry `admin`).
--
-- 3. forge.user_identities: only a person's account is linked to an IdP
--    identity (user_identities_human_only). The sign-in already refuses
--    other kinds (`not_human`); this keeps an operator link from making a
--    service account a sign-in target.
--
-- 4. forge.tokens.issued_by_user_id: the administrator who issued a token
--    for someone else (a service account's tokens, ADR-0019 Addendum S0);
--    NULL for a token its owner created and for the operator CLI's. The
--    audit trigger is re-created with the column in UPDATE OF (README);
--    forge_ro reads it (no secret).
--
-- 5. tokens_scopes_known gains `user_email` (the scope that lets a token
--    see people's e-mail addresses in API answers that offer them; it
--    satisfies no other scope and nothing satisfies it). The tokens table
--    is small, so the CHECK is replaced at once (20261111100000).

CREATE TABLE forge.service_account_profiles (
    user_id        bigint PRIMARY KEY REFERENCES forge.users (id),
    owner_user_id  bigint NOT NULL REFERENCES forge.users (id),
    purpose        text NOT NULL CONSTRAINT service_account_profiles_purpose_length
        CHECK (char_length(purpose) BETWEEN 1 AND 500),
    created_by     bigint REFERENCES forge.users (id),
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT service_account_profiles_not_self CHECK (owner_user_id <> user_id)
);
-- The accounts a person answers for (offboarding, doctor).
CREATE INDEX service_account_profiles_owner ON forge.service_account_profiles (owner_user_id);

CREATE FUNCTION forge.service_account_profiles_kinds() RETURNS trigger
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
REVOKE ALL ON FUNCTION forge.service_account_profiles_kinds() FROM PUBLIC;
CREATE TRIGGER service_account_profiles_kinds
    BEFORE INSERT OR UPDATE OF user_id, owner_user_id ON forge.service_account_profiles
    FOR EACH ROW EXECUTE FUNCTION forge.service_account_profiles_kinds();
ALTER TABLE forge.service_account_profiles ENABLE ALWAYS TRIGGER service_account_profiles_kinds;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF user_id, owner_user_id, purpose, created_by, created_at
    ON forge.service_account_profiles
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('user', 'user_id', 'updated_at', '');
ALTER TABLE forge.service_account_profiles ENABLE ALWAYS TRIGGER audited_change_present;

ALTER TABLE forge.users ADD CONSTRAINT users_service_account_no_personal_data CHECK (
    kind <> 'service_account'
    OR (email IS NULL AND employee_number IS NULL AND external_id IS NULL));
ALTER TABLE forge.users ADD CONSTRAINT users_service_account_not_admin CHECK (
    kind <> 'service_account' OR NOT is_admin);

CREATE FUNCTION forge.user_identities_human_only() RETURNS trigger
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
REVOKE ALL ON FUNCTION forge.user_identities_human_only() FROM PUBLIC;
CREATE TRIGGER user_identities_human_only
    BEFORE INSERT OR UPDATE OF user_id ON forge.user_identities
    FOR EACH ROW EXECUTE FUNCTION forge.user_identities_human_only();
ALTER TABLE forge.user_identities ENABLE ALWAYS TRIGGER user_identities_human_only;

ALTER TABLE forge.tokens ADD COLUMN issued_by_user_id bigint REFERENCES forge.users (id);
-- The tokens one administrator issued (the leak response's question).
CREATE INDEX tokens_issued_by ON forge.tokens (issued_by_user_id)
    WHERE issued_by_user_id IS NOT NULL;

DROP TRIGGER audited_change_present ON forge.tokens;
CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, user_id, public_id, secret_sha256, name, scopes,
        created_at, expires_at, revoked_at, revoked_reason, is_agent, issued_by_user_id
    ON forge.tokens
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('user', 'user_id', 'last_used_at', '');
ALTER TABLE forge.tokens ENABLE ALWAYS TRIGGER audited_change_present;

ALTER TABLE forge.tokens DROP CONSTRAINT tokens_scopes_known;
ALTER TABLE forge.tokens ADD CONSTRAINT tokens_scopes_known CHECK (
    cardinality(scopes) >= 1
    AND scopes <@ ARRAY['read_repository', 'write_repository', 'read_api', 'api', 'admin',
        'merge_execute', 'merge_approve', 'user_email']::text[]);

GRANT SELECT, INSERT, UPDATE ON forge.service_account_profiles TO forge_app;
GRANT SELECT ON forge.service_account_profiles TO forge_ro;
GRANT SELECT (issued_by_user_id) ON forge.tokens TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.service_account_profiles')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261113100000, "guarded_changes": ["forge.service_account_profiles.audited_change_present", "forge.tokens.audited_change_present"]}');
