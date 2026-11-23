-- ADR-0038 §2 (step B): access by organisation — an org binding gives the
-- people of an org unit (from the PFS org master's copy, 20261231100000) a
-- role on one group or project, beside the hand-made memberships.
--
-- forge.org_bindings (tables.toml kind = "derived"): what a PERSON does —
-- add or remove a binding (the group's or project's Owner, or an
-- administrator, in a web session; `OrgBindingManage`) — is audited by the
-- command in the same transaction (forge_db::org_bindings, ChangeTx:
-- `org_binding.created` / `org_binding.deleted` on the group or project).
--
--   group_id / project_id  the target: exactly one
--   target_kind/target_id  generated from them (the role loaders' lookup)
--   plant_code, org_code   the unit (its plant: a binding never spans plants)
--   include_descendants    also the units below it (by the master's parent
--                          chain, never by the code's shape)
--   membership             primary: the primary unit only; any: concurrent
--                          units too
--   min_rank               only people whose position_rank is at least this
--   role                   reporter | developer — never above Developer
--                          (ADR-0038 §2.1: Maintainer, Owner and deployment
--                          approval are given by hand only); the CHECK is
--                          the backstop of the command's FG-AUTH-0031
--   reason, created_by/_at the audit fields shown with the binding
--
-- A role from a binding is read by the role loaders (forge_db::roles,
-- `chain_role_columns!`) through forge.org_binding_role(), which applies
-- only when the person's snapshot (forge.user_org_profiles) is valid and
-- confirmed within the policy's age and the master's copy is fresh
-- (forge.org_access_policy, ADR-0038 §4): otherwise a binding gives nothing
-- and the hand-made memberships are unaffected.

CREATE TABLE forge.org_bindings (
    id                  bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    group_id            bigint REFERENCES forge.groups (id),
    project_id          bigint REFERENCES forge.projects (id),
    target_kind         text NOT NULL GENERATED ALWAYS AS
        (CASE WHEN group_id IS NOT NULL THEN 'group' ELSE 'project' END) STORED,
    target_id           bigint NOT NULL GENERATED ALWAYS AS (coalesce(group_id, project_id)) STORED,
    plant_code          text NOT NULL CONSTRAINT org_bindings_plant_code
        CHECK (plant_code ~ '^[A-Z0-9]{1,4}$'),
    org_code            text NOT NULL CONSTRAINT org_bindings_org_code
        CHECK (org_code ~ '^[A-Z0-9][A-Z0-9_-]{0,19}$'),
    include_descendants boolean NOT NULL DEFAULT false,
    membership          text NOT NULL DEFAULT 'any' CONSTRAINT org_bindings_membership
        CHECK (membership IN ('primary', 'any')),
    min_rank            integer CONSTRAINT org_bindings_min_rank
        CHECK (min_rank BETWEEN 0 AND 999),
    role                text NOT NULL CONSTRAINT org_bindings_role_cap
        CHECK (role IN ('reporter', 'developer')),
    reason              text CONSTRAINT org_bindings_reason
        CHECK (char_length(reason) BETWEEN 1 AND 500),
    created_by          bigint NOT NULL REFERENCES forge.users (id),
    created_at          timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT org_bindings_one_target CHECK (num_nonnulls(group_id, project_id) = 1)
);
-- The role loaders look a target up for every step of a chain.
CREATE INDEX org_bindings_target ON forge.org_bindings (target_kind, target_id);
-- The same unit, scope and condition twice on one target is one binding.
CREATE UNIQUE INDEX org_bindings_same ON forge.org_bindings
    (target_kind, target_id, plant_code, org_code, include_descendants, membership,
     coalesce(min_rank, -1));
CREATE INDEX org_bindings_project ON forge.org_bindings (project_id) WHERE project_id IS NOT NULL;
CREATE INDEX org_bindings_created_by ON forge.org_bindings (created_by);

-- How old the facts behind a binding may be (ADR-0038 §4): the configured
-- `org.max_profile_age_hours` / `org.master_max_age_hours`, written by
-- `forge serve` and `forge worker` at their start (forge_db::org_bindings::
-- publish_ages; one row, 24 h each until then — the configuration's
-- defaults). Derived state: a copy of the configuration.
CREATE TABLE forge.org_access_policy (
    id                   boolean PRIMARY KEY DEFAULT true CONSTRAINT org_access_policy_one CHECK (id),
    max_profile_age_secs integer NOT NULL CONSTRAINT org_access_policy_profile
        CHECK (max_profile_age_secs > 0),
    master_max_age_secs  integer NOT NULL CONSTRAINT org_access_policy_master
        CHECK (master_max_age_secs > 0),
    updated_at           timestamptz NOT NULL DEFAULT now()
);
INSERT INTO forge.org_access_policy (id, max_profile_age_secs, master_max_age_secs)
VALUES (true, 86400, 86400);

-- The bindings of (p_kind, p_target) that hold for p_user now, with their
-- roles (none when the facts cannot be trusted, ADR-0038 §4):
--   - the user is an active person with a snapshot that is valid
--     (invalid_since null), has a plant and was confirmed within the
--     policy's max_profile_age_secs;
--   - the master's copy was synced within master_max_age_secs;
--   - each of the person's units (primary and concurrent) is in the copy
--     (same plant, valid_from not in the future) and its parent chain is
--     whole: every parent present and valid, at a strictly higher level,
--     ending at a unit without a parent within 8 steps. A unit whose chain
--     is broken counts for nothing;
--   - a binding matches a unit on the chain: the unit itself, or one above
--     it when the binding includes descendants; `primary` bindings only the
--     primary unit's chain; `min_rank` against the snapshot's rank.
-- via_primary: the binding matches through the primary unit's chain (the
-- members list shows non-administrators only such matches of bindings
-- without min_rank: rank and concurrent units stay the person's and the
-- administrators', ADR-0038 §6).
-- SECURITY DEFINER: the snapshot's plant and rank are not on the read pool
-- (ADR-0038 §6). EXECUTE for forge_app only: per-binding matches of any
-- person are a rank oracle, so the read pool reaches them only through
-- forge.org_binding_role (the role, which the role loaders show anyway).
CREATE FUNCTION forge.org_binding_matches(p_user bigint, p_kind text, p_target bigint)
    RETURNS TABLE (binding_id bigint, binding_role text, via_primary boolean)
    LANGUAGE plpgsql STABLE
    SECURITY DEFINER
    SET search_path = pg_catalog, forge, pg_temp
AS $$
DECLARE
    v_units     jsonb;
    v_plant     text;
    v_rank      integer;
    v_invalid   timestamptz;
    v_confirmed timestamptz;
    v_profile   integer;
    v_master    integer;
    v_synced    timestamptz;
BEGIN
    IF p_user IS NULL OR NOT EXISTS (SELECT 1 FROM forge.org_bindings b
                                     WHERE b.target_kind = p_kind AND b.target_id = p_target) THEN
        RETURN;
    END IF;
    SELECT p.org_units, p.plant_code, p.position_rank, p.invalid_since, p.confirmed_at
      INTO v_units, v_plant, v_rank, v_invalid, v_confirmed
      FROM forge.user_org_profiles p JOIN forge.users u ON u.id = p.user_id
     WHERE p.user_id = p_user AND u.state = 'active' AND u.kind = 'human';
    IF NOT FOUND OR v_invalid IS NOT NULL OR v_plant IS NULL THEN
        RETURN;
    END IF;
    SELECT a.max_profile_age_secs, a.master_max_age_secs INTO v_profile, v_master
      FROM forge.org_access_policy a WHERE a.id;
    IF NOT FOUND OR v_confirmed < now() - make_interval(secs => v_profile) THEN
        RETURN;
    END IF;
    SELECT s.synced_at INTO v_synced FROM forge.org_master_state s WHERE s.id;
    IF v_synced IS NULL OR v_synced < now() - make_interval(secs => v_master) THEN
        RETURN;
    END IF;
    RETURN QUERY
    WITH RECURSIVE held (code, is_primary) AS (
        SELECT DISTINCT h.v ->> 'code', coalesce(h.v -> 'primary' = 'true'::jsonb, false)
          FROM jsonb_array_elements(v_units) AS h(v)
    ),
    chain (start, is_primary, depth, code, lvl, parent_code, ok) AS (
        SELECT m.code, h.is_primary, 0, m.code,
               CASE m.level WHEN 'team' THEN 1 WHEN 'section' THEN 2
                            WHEN 'department' THEN 3 WHEN 'division' THEN 4 END,
               m.parent_code, true
          FROM held h
          JOIN forge.org_units_master m ON m.plant_code = v_plant AND m.code = h.code
         WHERE m.valid_from IS NULL OR m.valid_from <= current_date
        UNION ALL
        SELECT ch.start, ch.is_primary, ch.depth + 1, m.code,
               CASE m.level WHEN 'team' THEN 1 WHEN 'section' THEN 2
                            WHEN 'department' THEN 3 WHEN 'division' THEN 4 END,
               m.parent_code,
               (m.valid_from IS NULL OR m.valid_from <= current_date)
                 AND (CASE m.level WHEN 'team' THEN 1 WHEN 'section' THEN 2
                                   WHEN 'department' THEN 3 WHEN 'division' THEN 4 END) > ch.lvl
          FROM chain ch
          JOIN forge.org_units_master m ON m.plant_code = v_plant AND m.code = ch.parent_code
         WHERE ch.ok AND ch.depth < 8
    ),
    whole (start, is_primary) AS (
        SELECT c.start, c.is_primary FROM chain c GROUP BY c.start, c.is_primary
        HAVING bool_and(c.ok) AND bool_or(c.parent_code IS NULL)
    )
    SELECT b.id, b.role, bool_or(w.is_primary)
      FROM whole w
      JOIN chain c ON c.start = w.start AND c.is_primary = w.is_primary
      JOIN forge.org_bindings b ON b.plant_code = v_plant AND b.org_code = c.code
     WHERE b.target_kind = p_kind AND b.target_id = p_target
       AND (c.depth = 0 OR b.include_descendants)
       AND (b.membership = 'any' OR w.is_primary)
       AND (b.min_rank IS NULL OR (v_rank IS NOT NULL AND v_rank >= b.min_rank))
       AND b.role IN ('reporter', 'developer')
     GROUP BY b.id, b.role;
END
$$;
REVOKE ALL ON FUNCTION forge.org_binding_matches(bigint, text, bigint) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION forge.org_binding_matches(bigint, text, bigint) TO forge_app;

-- The highest role the bindings of (p_kind, p_target) give p_user now
-- (null: none). Never above developer. SECURITY DEFINER: the read pool's
-- role loaders call it without EXECUTE on forge.org_binding_matches.
CREATE FUNCTION forge.org_binding_role(p_user bigint, p_kind text, p_target bigint)
    RETURNS text
    LANGUAGE sql STABLE
    SECURITY DEFINER
    SET search_path = pg_catalog, forge, pg_temp
AS $$
    SELECT CASE WHEN bool_or(m.binding_role = 'developer') THEN 'developer'
                WHEN count(*) > 0 THEN 'reporter' END
      FROM forge.org_binding_matches(p_user, p_kind, p_target) AS m
$$;
REVOKE ALL ON FUNCTION forge.org_binding_role(bigint, text, bigint) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION forge.org_binding_role(bigint, text, bigint) TO forge_app, forge_ro;

-- A membership's stored role raised by a binding's role (reporter or
-- developer): the higher of the two. A stored value outside the five roles
-- is kept as it is, so the role loaders still report it (FG-DB-0018).
CREATE FUNCTION forge.org_role_max(member text, bound text)
    RETURNS text
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
AS $$
    SELECT CASE WHEN bound IS NULL THEN member
                WHEN member IS NULL OR member = 'guest' THEN bound
                WHEN member = 'reporter' AND bound = 'developer' THEN bound
                ELSE member END
$$;
REVOKE ALL ON FUNCTION forge.org_role_max(text, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION forge.org_role_max(text, text) TO forge_app, forge_ro;

GRANT SELECT, INSERT, DELETE ON forge.org_bindings TO forge_app;
GRANT SELECT ON forge.org_bindings TO forge_ro;
GRANT SELECT, UPDATE ON forge.org_access_policy TO forge_app;
GRANT SELECT ON forge.org_access_policy TO forge_ro;

-- The members list (forge_db::org_bindings::members) narrows the people to
-- those holding a bound unit (or one below it) before matching them: the
-- containment probe `org_units @> '[{"code": …}]'` per unit.
CREATE INDEX user_org_profiles_units ON forge.user_org_profiles
    USING gin (org_units jsonb_path_ops);

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('function', 'forge.org_binding_matches(bigint,text,bigint)'),
    ('function', 'forge.org_binding_role(bigint,text,bigint)'),
    ('function', 'forge.org_role_max(text,text)')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20270101100000, "guarded_changes": ["forge.org_binding_matches", "forge.org_binding_role", "forge.org_role_max"]}');
