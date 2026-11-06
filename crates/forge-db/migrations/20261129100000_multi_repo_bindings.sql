-- M3 X3 (plan 11 §5.7 X3, plan 03 §7.1, plan 02 §2 / §5.3, ADR-0016
-- Amendment X3, ADR-0025 Amendment X3): a kanflow project with several
-- repositories, a shared spec repository in several kanflow projects, and
-- the binding assertions of acting for a person.
--
-- 1. forge.integration_bindings.resolves_work_items: whether the binding
--    resolves `PBI-123` in its project's branches and merge requests (the
--    work item detection of I1b, the dev status of I1d, the merge rule's
--    work_item_link). A binding that resolves is a project's only binding
--    (a repository with code is in one kanflow project: 02 §2); one that
--    does not (a shared spec repository, referenced by `specs:` of others)
--    may be bound to several kanflow projects. Existing bindings resolve
--    (true: what they did). The rule's backstop is the partial unique index
--    integration_bindings_one_resolving_per_project and the deferred
--    constraint trigger integration_bindings_resolving_alone (a project with
--    a resolving binding has no other binding, whichever row came first;
--    SQLSTATE 23514); the command checks the same under the project row
--    lock first (every bind locks it), which also orders concurrent binds.
--    forge_app may set it at the INSERT and UPDATE it (the command moves
--    it only false -> true: true -> false would leave the links I1b made;
--    unbinding ends them).
-- 2. The binding's id is its generation (an identity, new for every bind,
--    never changed): the primary key moves from (integration_id,
--    project_id) to generation, and (integration_id, project_id,
--    external_project_id) stays unique. integration_bindings_one_per_project
--    is replaced by the partial index of 1 and a plain lookup index.
-- 3. The audit trigger is re-created with resolves_work_items in UPDATE OF.
-- 4. forge.act_as_assertions gains the binding intents (ADR-0025 §4):
--    binding_set (bind or change a binding's settings: the external project
--    and resolves_work_items are sealed) and binding_remove (the external
--    project). The merge request columns are NULL for them and the binding
--    columns NULL for approve / unapprove (CHECK act_as_assertions_subject);
--    the guard keeps the new columns fixed like the rest of the binding.

-- ── 1. / 2. the bindings ────────────────────────────────────────────────
ALTER TABLE forge.integration_bindings
    ADD COLUMN resolves_work_items boolean NOT NULL DEFAULT true;

ALTER TABLE forge.integration_bindings DROP CONSTRAINT integration_bindings_pkey;
ALTER TABLE forge.integration_bindings
    ADD CONSTRAINT integration_bindings_pkey PRIMARY KEY (generation);
ALTER TABLE forge.integration_bindings
    ADD CONSTRAINT integration_bindings_one_per_external_project
    UNIQUE (integration_id, project_id, external_project_id);

DROP INDEX forge.integration_bindings_one_per_project;
CREATE UNIQUE INDEX integration_bindings_one_resolving_per_project
    ON forge.integration_bindings (project_id) WHERE resolves_work_items;
CREATE INDEX integration_bindings_project ON forge.integration_bindings (project_id);

-- A resolving binding is its project's only binding (review: the
-- schema's own backstop of the command's rule). Deferred, so a change that
-- removes the other rows in the same transaction passes; it reads the
-- project's rows as they are at the check.
CREATE FUNCTION forge.integration_bindings_resolving_alone() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM forge.integration_bindings
               WHERE project_id = NEW.project_id AND resolves_work_items)
       AND (SELECT count(*) FROM forge.integration_bindings
            WHERE project_id = NEW.project_id) > 1 THEN
        RAISE EXCEPTION USING ERRCODE = 'check_violation',
            CONSTRAINT = 'integration_bindings_resolving_alone',
            SCHEMA = TG_TABLE_SCHEMA, TABLE = TG_TABLE_NAME,
            MESSAGE = 'a binding that resolves work items on forge.integration_bindings is its project''s only binding';
    END IF;
    RETURN NULL;
END
$$;
REVOKE ALL ON FUNCTION forge.integration_bindings_resolving_alone() FROM PUBLIC;
CREATE CONSTRAINT TRIGGER integration_bindings_resolving_alone
    AFTER INSERT OR UPDATE OF project_id, resolves_work_items ON forge.integration_bindings
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.integration_bindings_resolving_alone();
ALTER TABLE forge.integration_bindings ENABLE ALWAYS TRIGGER integration_bindings_resolving_alone;

GRANT INSERT (resolves_work_items) ON forge.integration_bindings TO forge_app;
GRANT UPDATE (resolves_work_items) ON forge.integration_bindings TO forge_app;

-- ── 3. the bindings' audit trigger ─────────────────────────────────────
DROP TRIGGER audited_change_present ON forge.integration_bindings;
CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF integration_id, project_id, external_project_id,
        granted_membership, created_by, created_at, generation, resolves_work_items
    ON forge.integration_bindings
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.integration_bindings ENABLE ALWAYS TRIGGER audited_change_present;

-- ── 4. the binding assertions ──────────────────────────────────────────
ALTER TABLE forge.act_as_assertions DROP CONSTRAINT act_as_assertions_intent;
ALTER TABLE forge.act_as_assertions ADD CONSTRAINT act_as_assertions_intent
    CHECK (intent IN ('approve', 'unapprove', 'binding_set', 'binding_remove'));

ALTER TABLE forge.act_as_assertions
    ALTER COLUMN merge_request_id DROP NOT NULL,
    ALTER COLUMN head_sha DROP NOT NULL,
    ALTER COLUMN target_ref DROP NOT NULL,
    ALTER COLUMN target_sha DROP NOT NULL;

ALTER TABLE forge.act_as_assertions
    ADD COLUMN external_project_id text
        CONSTRAINT act_as_assertions_external_project_id
        CHECK (external_project_id ~ '^[A-Za-z0-9._:-]{1,100}$'),
    ADD COLUMN resolves_work_items boolean;

ALTER TABLE forge.act_as_assertions ADD CONSTRAINT act_as_assertions_subject CHECK (
    CASE
        WHEN intent IN ('approve', 'unapprove') THEN
            merge_request_id IS NOT NULL AND head_sha IS NOT NULL
            AND target_ref IS NOT NULL AND target_sha IS NOT NULL
            AND external_project_id IS NULL AND resolves_work_items IS NULL
        WHEN intent = 'binding_set' THEN
            merge_request_id IS NULL AND head_sha IS NULL
            AND target_ref IS NULL AND target_sha IS NULL
            AND external_project_id IS NOT NULL AND resolves_work_items IS NOT NULL
        ELSE
            merge_request_id IS NULL AND head_sha IS NULL
            AND target_ref IS NULL AND target_sha IS NULL
            AND external_project_id IS NOT NULL AND resolves_work_items IS NULL
    END);

CREATE OR REPLACE FUNCTION forge.act_as_assertions_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM forge.users WHERE id = NEW.user_id AND kind = 'human') THEN
        RAISE EXCEPTION USING ERRCODE = 'check_violation',
            CONSTRAINT = 'act_as_assertions_user_human',
            SCHEMA = TG_TABLE_SCHEMA, TABLE = TG_TABLE_NAME,
            MESSAGE = 'an assertion on forge.act_as_assertions is a person''s';
    END IF;
    IF TG_OP = 'INSERT' THEN
        IF NEW.state <> 'pending' THEN
            RAISE EXCEPTION USING ERRCODE = 'XF001',
                MESSAGE = 'an assertion on forge.act_as_assertions starts pending';
        END IF;
        RETURN NEW;
    END IF;
    IF NEW.integration_id IS DISTINCT FROM OLD.integration_id
       OR NEW.user_id IS DISTINCT FROM OLD.user_id
       OR NEW.intent IS DISTINCT FROM OLD.intent
       OR NEW.project_id IS DISTINCT FROM OLD.project_id
       OR NEW.merge_request_id IS DISTINCT FROM OLD.merge_request_id
       OR NEW.head_sha IS DISTINCT FROM OLD.head_sha
       OR NEW.target_ref IS DISTINCT FROM OLD.target_ref
       OR NEW.target_sha IS DISTINCT FROM OLD.target_sha
       OR NEW.external_project_id IS DISTINCT FROM OLD.external_project_id
       OR NEW.resolves_work_items IS DISTINCT FROM OLD.resolves_work_items
       OR NEW.state_hash IS DISTINCT FROM OLD.state_hash
       OR NEW.issued_at IS DISTINCT FROM OLD.issued_at
       OR NEW.expires_at IS DISTINCT FROM OLD.expires_at
       OR NEW.issued_request_id IS DISTINCT FROM OLD.issued_request_id THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the binding of an assertion on forge.act_as_assertions never changes';
    END IF;
    IF OLD.mac IS NOT NULL
       AND (NEW.mac IS DISTINCT FROM OLD.mac OR NEW.key_id IS DISTINCT FROM OLD.key_id
            OR NEW.public_id IS DISTINCT FROM OLD.public_id
            OR NEW.claimed_at IS DISTINCT FROM OLD.claimed_at
            OR NEW.claimed_request_id IS DISTINCT FROM OLD.claimed_request_id) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a claimed assertion on forge.act_as_assertions keeps its claim';
    END IF;
    IF NOT ((OLD.state = 'pending' AND NEW.state IN ('claimed', 'rejected'))
            OR (OLD.state = 'claimed' AND NEW.state IN ('consumed', 'rejected'))) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'an assertion on forge.act_as_assertions only moves pending -> claimed -> consumed / rejected';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.act_as_assertions_guard() FROM PUBLIC;

DROP TRIGGER audited_change_present ON forge.act_as_assertions;
CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, state, public_id, mac, key_id, integration_id,
        user_id, intent, project_id, merge_request_id, head_sha, target_ref, target_sha,
        external_project_id, resolves_work_items,
        state_hash, issued_at, expires_at, claimed_at, finished_at, reject_reason,
        issued_request_id, claimed_request_id, finished_request_id
    ON forge.act_as_assertions
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('act_as_assertion', 'id', '', '');
ALTER TABLE forge.act_as_assertions ENABLE ALWAYS TRIGGER audited_change_present;

GRANT INSERT (external_project_id, resolves_work_items) ON forge.act_as_assertions TO forge_app;
GRANT SELECT (external_project_id, resolves_work_items) ON forge.act_as_assertions TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('function', 'forge.integration_bindings_resolving_alone()'),
    ('trigger', 'integration_bindings_resolving_alone on forge.integration_bindings')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261129100000, "guarded_changes": ["forge.integration_bindings", "forge.integration_bindings.audited_change_present", "forge.integration_bindings_resolving_alone", "forge.integration_bindings.integration_bindings_resolving_alone", "forge.act_as_assertions", "forge.act_as_assertions_guard", "forge.act_as_assertions.audited_change_present"]}');
