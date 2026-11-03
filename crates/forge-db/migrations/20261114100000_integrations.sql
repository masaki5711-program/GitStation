-- M3 I1a (plan 11 §5 I1, 02 §2, ADR-0016 §1 / §7, ADR-0016 Amendment
-- I1a): integrations and their project bindings.
--
-- 1. integrations: one connected system (kind, today only 'kanflow') and
--    the service account it acts as (one integration per service account,
--    integrations_one_per_service_account; plan 11 Q4: one account per
--    purpose). event_types is the allowlist the event feed (I1c) narrows
--    by ('*', a type or a 'family.*' of the integration class; checked by
--    forge-core, at most 64 entries here). No secret columns yet: the
--    inbound secret arrives with the inbound events (ADR-0016 §8).
--    The guard integrations_service_account_guard keeps the account a
--    service account (constraint name integrations_service_account_kind;
--    users.kind never changes, users_kind_immutable) and fixed for the
--    integration's life (integrations_service_account_fixed): the
--    memberships its bindings granted belong to that account. Audited on
--    ('integration', id): integration.created / integration.updated. No
--    DELETE (an integration is disabled, never removed).
--
-- 2. integration_bindings: one forge project bound to one external
--    (kanflow) project of one integration. At most one binding per project
--    (integration_bindings_one_per_project: a project in two kanflow
--    projects makes `PBI-123` ambiguous, 02 §2), at most 200 per
--    integration (the command counts under the integration's row lock,
--    FG-INT-0012). granted_membership records whether binding made the
--    service account a Reporter, so unbinding removes only what it added.
--    Audited on ('project', project_id): integration.bound /
--    integration.unbound. Deleted by unbinding and by the project purge.

CREATE TABLE forge.integrations (
    id                  bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    kind                text NOT NULL CONSTRAINT integrations_kind_known
        CHECK (kind IN ('kanflow')),
    service_account_id  bigint NOT NULL REFERENCES forge.users (id),
    enabled             boolean NOT NULL DEFAULT true,
    event_types         text[] NOT NULL CONSTRAINT integrations_event_types_count
        CHECK (cardinality(event_types) BETWEEN 1 AND 64),
    created_by          bigint REFERENCES forge.users (id),
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT integrations_one_per_service_account UNIQUE (service_account_id)
);

CREATE FUNCTION forge.integrations_service_account_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP = 'UPDATE' AND NEW.service_account_id <> OLD.service_account_id THEN
        RAISE EXCEPTION USING ERRCODE = 'check_violation',
            CONSTRAINT = 'integrations_service_account_fixed',
            SCHEMA = TG_TABLE_SCHEMA, TABLE = TG_TABLE_NAME,
            MESSAGE = 'an integration keeps its service account';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM forge.users
                   WHERE id = NEW.service_account_id AND kind = 'service_account') THEN
        RAISE EXCEPTION USING ERRCODE = 'check_violation',
            CONSTRAINT = 'integrations_service_account_kind',
            SCHEMA = TG_TABLE_SCHEMA, TABLE = TG_TABLE_NAME,
            MESSAGE = 'an integration acts as a service account';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.integrations_service_account_guard() FROM PUBLIC;
CREATE TRIGGER integrations_service_account_guard
    BEFORE INSERT OR UPDATE OF service_account_id ON forge.integrations
    FOR EACH ROW EXECUTE FUNCTION forge.integrations_service_account_guard();
ALTER TABLE forge.integrations ENABLE ALWAYS TRIGGER integrations_service_account_guard;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, kind, service_account_id, enabled, event_types,
        created_by, created_at
    ON forge.integrations
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('integration', 'id', 'updated_at', '');
ALTER TABLE forge.integrations ENABLE ALWAYS TRIGGER audited_change_present;

CREATE TABLE forge.integration_bindings (
    integration_id       bigint NOT NULL REFERENCES forge.integrations (id),
    project_id           bigint NOT NULL REFERENCES forge.projects (id),
    external_project_id  text NOT NULL
        CONSTRAINT integration_bindings_external_project_id_format
        CHECK (external_project_id ~ '^[A-Za-z0-9._:-]{1,100}$'),
    granted_membership   boolean NOT NULL,
    created_by           bigint REFERENCES forge.users (id),
    created_at           timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (integration_id, project_id)
);
-- One binding per project (02 §2); also the project's lookup.
CREATE UNIQUE INDEX integration_bindings_one_per_project
    ON forge.integration_bindings (project_id);

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF integration_id, project_id, external_project_id,
        granted_membership, created_by, created_at
    ON forge.integration_bindings
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.integration_bindings ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT, INSERT, UPDATE ON forge.integrations TO forge_app;
GRANT SELECT ON forge.integrations TO forge_ro;
GRANT SELECT, INSERT, DELETE ON forge.integration_bindings TO forge_app;
GRANT SELECT ON forge.integration_bindings TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('function', 'forge.integrations_service_account_guard()'),
    ('trigger', 'integrations_service_account_guard on forge.integrations'),
    ('trigger', 'audited_change_present on forge.integrations'),
    ('trigger', 'audited_change_present on forge.integration_bindings')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261114100000, "guarded_changes": ["forge.integrations_service_account_guard", "forge.integrations.integrations_service_account_guard", "forge.integrations.audited_change_present", "forge.integration_bindings.audited_change_present"]}');
