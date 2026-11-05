-- M3 S2 (ADR-0025, plan 11 §5 S2): acting for a person (kanflow approves
-- for the person who pressed forge's confirmation).
--
-- 1. The token scope `act_as` (ADR-0025 §2): tokens_scopes_known gains it
--    (the variant and its CHECK land in the same MR; replaced at once, the
--    table is small). forge-db's tokens::issue gives it to a service
--    account's token only, never with is_agent.
--
-- 2. users_employee_number_kept_when_disabled (ADR-0025 §1, an acceptance
--    condition): a disabled user's employee_number never becomes NULL or
--    another value, and disabling never changes it in the same UPDATE, so
--    the unique index users_employee_number keeps counting a leaver's
--    number and it never reaches a new person (`Forge-Act-As:
--    employee:<n>` resolves to exactly one row or nobody). SQLSTATE 23514,
--    constraint name users_employee_number_kept_when_disabled. A guard of a
--    state table (tables.toml guarded_functions; doctor compares it).
--
-- 3. act_as_assertions (ADR-0025 §5.1): one row per confirmation. It moves
--    forward only: pending (issued by the person's session: the binding
--    and state_hash, no secret, no public_id, no mac) → claimed (the
--    integration's claim: public_id, mac and key_id written, the secret
--    returned once and never stored) → consumed (used by the action, in
--    its transaction) or rejected (verified but not used, or the pending
--    row expired). `mac = HMAC(K, purpose ‖ secret ‖ binding)` with K
--    outside the database, so a row written or changed with forge_app's
--    credentials never verifies. No DELETE (retention comes later, like
--    O3's); no foreign key to projects / merge_requests (the project purge
--    deletes those rows, these stay as the audit's companions). Audited on
--    ('act_as_assertion', id): act_as_assertion.issued / .claimed /
--    .consumed / .rejected. forge_ro reads neither mac nor state_hash.
--    The guard act_as_assertions_guard keeps the state machine, the
--    binding and the person (users.kind = 'human').
--
-- 4. merge_request_approvals.credential_origin (ADR-0025 §4, §6; ADR-0017
--    §4.2): `session`, `token` or `act_as_assertion` (NULL: written before
--    this migration). An act-as approval is never carried to a new head
--    (forge-db review::approvals::carry_on ends it with new_commits
--    whatever the project's setting). Part of the approval's identity
--    (the guard is replaced with the column added) and of the audit
--    trigger's columns.

ALTER TABLE forge.tokens DROP CONSTRAINT tokens_scopes_known;
ALTER TABLE forge.tokens ADD CONSTRAINT tokens_scopes_known CHECK (
    cardinality(scopes) >= 1
    AND scopes <@ ARRAY['read_repository', 'write_repository', 'read_api', 'api', 'admin',
        'merge_execute', 'merge_approve', 'user_email', 'user_employee_number', 'act_as']::text[]);

-- ── 2. a disabled user's employee number stays ─────────────────────────
CREATE FUNCTION forge.users_employee_number_kept_when_disabled() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF (OLD.state = 'disabled' OR NEW.state = 'disabled')
       AND NEW.employee_number IS DISTINCT FROM OLD.employee_number THEN
        RAISE EXCEPTION USING ERRCODE = 'check_violation',
            CONSTRAINT = 'users_employee_number_kept_when_disabled',
            SCHEMA = TG_TABLE_SCHEMA, TABLE = TG_TABLE_NAME,
            MESSAGE = 'a disabled user on forge.users keeps its employee number';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.users_employee_number_kept_when_disabled() FROM PUBLIC;
CREATE TRIGGER users_employee_number_kept_when_disabled
    BEFORE UPDATE OF employee_number, state ON forge.users
    FOR EACH ROW EXECUTE FUNCTION forge.users_employee_number_kept_when_disabled();
ALTER TABLE forge.users ENABLE ALWAYS TRIGGER users_employee_number_kept_when_disabled;

-- ── 3. the assertions ──────────────────────────────────────────────────
CREATE TABLE forge.act_as_assertions (
    id                   bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    state                text NOT NULL DEFAULT 'pending' CONSTRAINT act_as_assertions_state
        CHECK (state IN ('pending', 'claimed', 'consumed', 'rejected')),
    public_id            text CONSTRAINT act_as_assertions_public_id_key UNIQUE
        CONSTRAINT act_as_assertions_public_id CHECK (public_id ~ '^[0-9a-f]{16}$'),
    mac                  bytea CONSTRAINT act_as_assertions_mac CHECK (octet_length(mac) = 32),
    key_id               smallint CONSTRAINT act_as_assertions_key_id CHECK (key_id >= 1),
    integration_id       bigint NOT NULL REFERENCES forge.integrations (id),
    user_id              bigint NOT NULL REFERENCES forge.users (id),
    intent               text NOT NULL CONSTRAINT act_as_assertions_intent
        CHECK (intent IN ('approve', 'unapprove')),
    project_id           bigint NOT NULL,
    merge_request_id     bigint NOT NULL,
    head_sha             text NOT NULL CONSTRAINT act_as_assertions_head_sha
        CHECK (head_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    target_ref           text COLLATE "C" NOT NULL CONSTRAINT act_as_assertions_target_ref
        CHECK (target_ref ~ '^refs/heads/.' AND octet_length(target_ref) <= 1024),
    target_sha           text NOT NULL CONSTRAINT act_as_assertions_target_sha
        CHECK (target_sha ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    state_hash           bytea NOT NULL CONSTRAINT act_as_assertions_state_hash
        CHECK (octet_length(state_hash) = 32),
    issued_at            timestamptz NOT NULL DEFAULT now(),
    expires_at           timestamptz NOT NULL,
    claimed_at           timestamptz,
    finished_at          timestamptz,
    reject_reason        text CONSTRAINT act_as_assertions_reject_reason
        CHECK (reject_reason ~ '^[a-z][a-z0-9_]{0,63}$'),
    issued_request_id    text NOT NULL CONSTRAINT act_as_assertions_issued_request_id
        CHECK (length(issued_request_id) BETWEEN 1 AND 128),
    claimed_request_id   text CONSTRAINT act_as_assertions_claimed_request_id
        CHECK (length(claimed_request_id) BETWEEN 1 AND 128),
    finished_request_id  text CONSTRAINT act_as_assertions_finished_request_id
        CHECK (length(finished_request_id) BETWEEN 1 AND 128),
    CONSTRAINT act_as_assertions_lifetime
        CHECK (expires_at > issued_at AND expires_at <= issued_at + interval '5 minutes'),
    -- The secret's traces exist exactly from the claim on: a pending row
    -- has none, a claimed or consumed one all three; a rejected one either
    -- (an expired pending row is rejected without ever being claimed).
    CONSTRAINT act_as_assertions_mac_key CHECK ((mac IS NULL) = (key_id IS NULL)),
    CONSTRAINT act_as_assertions_mac_public_id CHECK ((mac IS NULL) = (public_id IS NULL)),
    CONSTRAINT act_as_assertions_mac_claimed CHECK ((mac IS NULL) = (claimed_at IS NULL)),
    CONSTRAINT act_as_assertions_pending_without_secret
        CHECK (state <> 'pending' OR mac IS NULL),
    CONSTRAINT act_as_assertions_claimed_with_secret
        CHECK (state NOT IN ('claimed', 'consumed') OR mac IS NOT NULL),
    CONSTRAINT act_as_assertions_finished
        CHECK ((finished_at IS NOT NULL) = (state IN ('consumed', 'rejected'))),
    CONSTRAINT act_as_assertions_rejected
        CHECK ((reject_reason IS NOT NULL) = (state = 'rejected'))
);
-- The claim looks a pending row up by its state.
CREATE INDEX act_as_assertions_state_hash ON forge.act_as_assertions (state_hash)
    WHERE state = 'pending';
-- The cleanup of expired pending rows; the audit's per-person review.
CREATE INDEX act_as_assertions_pending_expiry ON forge.act_as_assertions (expires_at)
    WHERE state = 'pending';
CREATE INDEX act_as_assertions_user ON forge.act_as_assertions (user_id, issued_at);

CREATE FUNCTION forge.act_as_assertions_guard() RETURNS trigger
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
CREATE TRIGGER act_as_assertions_forward BEFORE INSERT OR UPDATE ON forge.act_as_assertions
    FOR EACH ROW EXECUTE FUNCTION forge.act_as_assertions_guard();
ALTER TABLE forge.act_as_assertions ENABLE ALWAYS TRIGGER act_as_assertions_forward;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, state, public_id, mac, key_id, integration_id,
        user_id, intent, project_id, merge_request_id, head_sha, target_ref, target_sha,
        state_hash, issued_at, expires_at, claimed_at, finished_at, reject_reason,
        issued_request_id, claimed_request_id, finished_request_id
    ON forge.act_as_assertions
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('act_as_assertion', 'id', '', '');
ALTER TABLE forge.act_as_assertions ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT ON forge.act_as_assertions TO forge_app;
GRANT INSERT (integration_id, user_id, intent, project_id, merge_request_id, head_sha,
    target_ref, target_sha, state_hash, expires_at, issued_request_id)
    ON forge.act_as_assertions TO forge_app;
GRANT UPDATE (state, public_id, mac, key_id, claimed_at, finished_at, reject_reason,
    claimed_request_id, finished_request_id) ON forge.act_as_assertions TO forge_app;
GRANT SELECT (id, state, public_id, key_id, integration_id, user_id, intent, project_id,
    merge_request_id, head_sha, target_ref, target_sha, issued_at, expires_at, claimed_at,
    finished_at, reject_reason, issued_request_id, claimed_request_id, finished_request_id)
    ON forge.act_as_assertions TO forge_ro;

-- ── 4. where an approval came from ─────────────────────────────────────
ALTER TABLE forge.merge_request_approvals ADD COLUMN credential_origin text
    CONSTRAINT merge_request_approvals_credential_origin
    CHECK (credential_origin IN ('session', 'token', 'act_as_assertion'));

-- An approval's identity (now with credential_origin) never changes; it
-- ends once and never comes back.
CREATE OR REPLACE FUNCTION forge.merge_request_approvals_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NEW.merge_request_id IS DISTINCT FROM OLD.merge_request_id
       OR NEW.project_id IS DISTINCT FROM OLD.project_id
       OR NEW.approver_id IS DISTINCT FROM OLD.approver_id
       OR NEW.head_sha IS DISTINCT FROM OLD.head_sha
       OR NEW.target_branch IS DISTINCT FROM OLD.target_branch
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
       OR NEW.carried_from IS DISTINCT FROM OLD.carried_from
       OR NEW.credential_origin IS DISTINCT FROM OLD.credential_origin THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the identity of an approval on forge.merge_request_approvals never changes';
    END IF;
    IF OLD.revoked_at IS NOT NULL AND NEW IS DISTINCT FROM OLD THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'an ended approval on forge.merge_request_approvals never changes';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.merge_request_approvals_guard() FROM PUBLIC;

DROP TRIGGER audited_change_present ON forge.merge_request_approvals;
CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, merge_request_id, project_id, approver_id, head_sha,
        target_branch, created_at, revoked_at, revoked_reason, carried_from, credential_origin
    ON forge.merge_request_approvals
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.merge_request_approvals ENABLE ALWAYS TRIGGER audited_change_present;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('function', 'forge.users_employee_number_kept_when_disabled()'),
    ('trigger', 'users_employee_number_kept_when_disabled on forge.users'),
    ('function', 'forge.act_as_assertions_guard()'),
    ('trigger', 'act_as_assertions_forward on forge.act_as_assertions'),
    ('trigger', 'audited_change_present on forge.act_as_assertions')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261121100000, "guarded_changes": ["forge.users_employee_number_kept_when_disabled", "forge.users.users_employee_number_kept_when_disabled", "forge.act_as_assertions_guard", "forge.act_as_assertions.act_as_assertions_forward", "forge.act_as_assertions.audited_change_present", "forge.merge_request_approvals_guard", "forge.merge_request_approvals.audited_change_present"]}');
