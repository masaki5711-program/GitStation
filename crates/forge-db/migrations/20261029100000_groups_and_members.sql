-- Step D2 (ADR-0010, ADR-0015, ADR-0016 §1): groups and their members. Both
-- are state tables (tables.toml kind = "audited"): every change commits only
-- with a succeeded audit row for the group (forge.audited_change_present,
-- migration 20261028100000).
--
-- groups: one row per group. Its path lives in forge.namespaces (kind =
-- 'group', owner_id = groups.id), so one `group` audit row covers both. No
-- DELETE grant: groups are not deleted in M1 (step D plan Q11).
--
-- group_members: direct memberships (a user's role in one group). Inherited
-- roles are computed, never stored (the D2 role loader). Bots are never
-- members (ADR-0017: they act for a person); forge-db groups enforces it
-- under the group row lock, together with "a top-level group keeps at least
-- one direct active Owner". member.* changes are audit-only (Q8).
--
-- No role gets TRUNCATE (it would bypass the row triggers); doctor compares
-- the exact ACLs (bootstrap/schema.rs).

CREATE TABLE forge.groups (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    display_name  text NOT NULL CONSTRAINT groups_display_name_length
        CHECK (length(display_name) BETWEEN 1 AND 255),
    description   text NOT NULL DEFAULT '' CONSTRAINT groups_description_length
        CHECK (length(description) <= 2000),
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE forge.group_members (
    group_id    bigint NOT NULL REFERENCES forge.groups (id),
    user_id     bigint NOT NULL REFERENCES forge.users (id),
    role        text NOT NULL CONSTRAINT group_members_role
        CHECK (role IN ('guest', 'reporter', 'developer', 'maintainer', 'owner')),
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT group_members_pkey PRIMARY KEY (group_id, user_id)
);
-- The role loader looks memberships up by user.
CREATE INDEX group_members_user ON forge.group_members (user_id);

-- Bots are never members, also for a writer that skips forge-db groups
-- (forge-db never changes users.kind, so checking on insert and on a changed
-- user_id suffices; a kind change would need its own check).
CREATE FUNCTION forge.group_members_no_bot() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM forge.users WHERE id = NEW.user_id AND kind = 'bot') THEN
        RAISE EXCEPTION USING ERRCODE = 'check_violation', CONSTRAINT = 'group_members_no_bot',
            SCHEMA = TG_TABLE_SCHEMA, TABLE = TG_TABLE_NAME,
            MESSAGE = 'a bot cannot be a group member';
    END IF;
    RETURN NEW;
END
$$;
CREATE TRIGGER group_members_no_bot
    BEFORE INSERT OR UPDATE OF user_id ON forge.group_members
    FOR EACH ROW EXECUTE FUNCTION forge.group_members_no_bot();
ALTER TABLE forge.group_members ENABLE ALWAYS TRIGGER group_members_no_bot;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, display_name, description, created_at
    ON forge.groups
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('group', 'id', 'updated_at', '');
ALTER TABLE forge.groups ENABLE ALWAYS TRIGGER audited_change_present;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF group_id, user_id, role, created_at
    ON forge.group_members
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('group', 'group_id', 'updated_at', '');
ALTER TABLE forge.group_members ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT, INSERT, UPDATE ON forge.groups TO forge_app;
GRANT SELECT ON forge.groups TO forge_ro;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.group_members TO forge_app;
GRANT SELECT ON forge.group_members TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.groups'),
    ('trigger', 'audited_change_present on forge.group_members')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261029100000, "guarded_changes": ["forge.groups.audited_change_present", "forge.group_members.audited_change_present"]}');
