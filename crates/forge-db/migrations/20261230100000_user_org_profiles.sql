-- ADR-0021 amendment 2026-11-20 (組織の claim の写し): a snapshot of the PFS
-- Keycloak's organisation claims of each person, written at the sign-in and
-- at every verified refresh check (`auth.oidc.claims_source =
-- access_token` only). Display only in Phase 1: nothing is authorized by it.
--
-- forge.user_org_profiles (tables.toml kind = "audited", on ('user',
-- user_id)): one row per user.
--
--   issuer         the identity provider the claims came from
--   org_units      [{"code": "...", "primary": true|false}], at most 64, at
--                  most one primary (the primary first)
--   positions      position codes, at most 64
--   position_rank  0..=999, or null
--   plants         plant codes, at most 64
--   plant_code     the person's own plant, or null
--   digest         SHA-256 (hex) of the canonical snapshot: an equal digest
--                  at the next sign-in moves confirmed_at only
--   confirmed_at   the last time the provider sent these values (or sent
--                  values that failed validation: then invalid_since)
--   changed_at     the last time the values changed (audited)
--   invalid_since  the provider's claims fail validation since then; the
--                  last valid values stay (null: the last claims were valid)
--
-- confirmed_at and invalid_since are unaudited; every other change commits
-- with a `user.org_profile_changed` audit row on the user.

-- The table's CHECKs mirror forge-oidc's validation (the backstop: a
-- write that bypasses it is refused). A CHECK cannot hold a subquery, so
-- the element rules live in two IMMUTABLE helpers (not SECURITY DEFINER;
-- pinned search_path).
--
-- org_units: an array of at most 64 objects, each exactly {"code": "...",
-- "primary": true|false} with code ^[A-Z0-9][A-Z0-9_-]{0,19}$, and at most
-- one primary.
CREATE FUNCTION forge.org_units_valid(units jsonb) RETURNS boolean
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    SET search_path = pg_catalog, pg_temp
AS $$
    SELECT CASE WHEN jsonb_typeof(units) <> 'array' THEN false
                WHEN jsonb_array_length(units) > 64 THEN false
                ELSE NOT EXISTS (
                         SELECT 1 FROM jsonb_array_elements(units) AS u(v)
                         WHERE CASE WHEN jsonb_typeof(u.v) <> 'object' THEN true
                                    ELSE jsonb_typeof(u.v -> 'code') IS DISTINCT FROM 'string'
                                      OR NOT ((u.v ->> 'code') ~ '^[A-Z0-9][A-Z0-9_-]{0,19}$')
                                      OR jsonb_typeof(u.v -> 'primary') IS DISTINCT FROM 'boolean'
                                      OR (u.v - 'code' - 'primary') <> '{}'::jsonb
                               END)
                     AND (SELECT count(*) FROM jsonb_array_elements(units) AS u(v)
                          WHERE jsonb_typeof(u.v) = 'object'
                            AND u.v -> 'primary' = 'true'::jsonb) <= 1
           END
$$;

-- Every element of codes is non-null and matches pattern.
CREATE FUNCTION forge.codes_match(codes text[], pattern text) RETURNS boolean
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    SET search_path = pg_catalog, pg_temp
AS $$
    SELECT NOT EXISTS (SELECT 1 FROM unnest(codes) AS c(v) WHERE c.v IS NULL OR c.v !~ pattern)
$$;

CREATE TABLE forge.user_org_profiles (
    user_id        bigint PRIMARY KEY REFERENCES forge.users (id),
    issuer         text NOT NULL CONSTRAINT user_org_profiles_issuer_length
        CHECK (char_length(issuer) BETWEEN 1 AND 255),
    org_units      jsonb NOT NULL DEFAULT '[]'::jsonb CONSTRAINT user_org_profiles_org_units
        CHECK (forge.org_units_valid(org_units)),
    positions      text[] NOT NULL DEFAULT '{}' CONSTRAINT user_org_profiles_positions
        CHECK (cardinality(positions) <= 64
               AND forge.codes_match(positions, '^[a-z][a-z0-9_]{0,63}$')),
    position_rank  integer CONSTRAINT user_org_profiles_position_rank
        CHECK (position_rank BETWEEN 0 AND 999),
    plants         text[] NOT NULL DEFAULT '{}' CONSTRAINT user_org_profiles_plants
        CHECK (cardinality(plants) <= 64 AND forge.codes_match(plants, '^[A-Z0-9]{1,4}$')),
    plant_code     text CONSTRAINT user_org_profiles_plant_code
        CHECK (plant_code ~ '^[A-Z0-9]{1,4}$'),
    digest         text NOT NULL CONSTRAINT user_org_profiles_digest
        CHECK (digest ~ '^[0-9a-f]{64}$'),
    confirmed_at   timestamptz NOT NULL DEFAULT now(),
    changed_at     timestamptz NOT NULL DEFAULT now(),
    invalid_since  timestamptz
);
-- doctor: the invalid snapshots.
CREATE INDEX user_org_profiles_invalid ON forge.user_org_profiles (invalid_since)
    WHERE invalid_since IS NOT NULL;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF user_id, issuer, org_units, positions, position_rank,
        plants, plant_code, digest, changed_at
    ON forge.user_org_profiles
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('user', 'user_id',
        'confirmed_at,invalid_since', '');
ALTER TABLE forge.user_org_profiles ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT, INSERT, UPDATE ON forge.user_org_profiles TO forge_app;
-- The CHECKs run the helpers as the writer (PUBLIC has no EXECUTE here).
GRANT EXECUTE ON FUNCTION forge.org_units_valid(jsonb), forge.codes_match(text[], text)
    TO forge_app;
-- The read pool: the department codes (the people pickers' primary code)
-- and doctor's columns; never the positions, the rank or the plants.
GRANT SELECT (user_id, org_units, confirmed_at, invalid_since)
    ON forge.user_org_profiles TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.user_org_profiles')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261230100000, "guarded_changes": ["forge.user_org_profiles.audited_change_present"]}');
