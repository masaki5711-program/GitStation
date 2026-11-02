-- Step D3 (ADR-0019, ADR-0018, ADR-0016 §1): personal access tokens. A state
-- table (tables.toml kind = "audited"): every change commits only with a
-- succeeded audit row for the owner (resource 'user', id = user_id;
-- token.created / token.revoked also publish a domain event, ADR-0016 §10).
--
-- A token is fgp_<public_id>_<secret>. Only sha256(secret) is stored
-- (secret_sha256); forge_ro never sees it (ADR-0018: column grant without
-- secret_sha256). forge_app may insert and may update revoked_at,
-- revoked_reason and last_used_at only; no DELETE (a revoked token stays as
-- history). last_used_at is unaudited (written at most every 10 minutes).
-- No role gets TRUNCATE.

CREATE TABLE forge.tokens (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id        bigint NOT NULL REFERENCES forge.users (id),
    public_id      text NOT NULL,
    secret_sha256  bytea NOT NULL,
    name           text NOT NULL,
    scopes         text[] NOT NULL,
    created_at     timestamptz NOT NULL DEFAULT now(),
    expires_at     timestamptz NOT NULL,
    revoked_at     timestamptz,
    revoked_reason text,
    last_used_at   timestamptz,
    -- ADR-0017 §4.0: a token held by an AI agent (default false).
    is_agent       boolean NOT NULL DEFAULT false,
    CONSTRAINT tokens_public_id_key UNIQUE (public_id),
    CONSTRAINT tokens_public_id_hex CHECK (public_id ~ '^[0-9a-f]{16}$'),
    CONSTRAINT tokens_secret_sha256_len CHECK (octet_length(secret_sha256) = 32),
    CONSTRAINT tokens_name_length CHECK (char_length(name) BETWEEN 1 AND 100),
    CONSTRAINT tokens_scopes_known CHECK (
        cardinality(scopes) >= 1
        AND scopes <@ ARRAY['read_repository', 'write_repository', 'read_api', 'api', 'admin']::text[]),
    CONSTRAINT tokens_expiry_range CHECK (
        expires_at > created_at AND expires_at <= created_at + interval '365 days'),
    CONSTRAINT tokens_revoked_reason CHECK (
        revoked_reason IS NULL OR revoked_reason IN ('revoked', 'user_disabled')),
    CONSTRAINT tokens_revoked_pair CHECK ((revoked_at IS NULL) = (revoked_reason IS NULL))
);
-- The active-token count and the owner's list.
CREATE INDEX tokens_user_active ON forge.tokens (user_id) WHERE revoked_at IS NULL;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, user_id, public_id, secret_sha256, name, scopes,
        created_at, expires_at, revoked_at, revoked_reason, is_agent
    ON forge.tokens
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('user', 'user_id', 'last_used_at', '');
ALTER TABLE forge.tokens ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT, INSERT ON forge.tokens TO forge_app;
GRANT UPDATE (revoked_at, revoked_reason, last_used_at) ON forge.tokens TO forge_app;
GRANT SELECT (id, user_id, public_id, name, scopes, created_at, expires_at, revoked_at,
    revoked_reason, last_used_at, is_agent) ON forge.tokens TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.tokens')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261030100000, "guarded_changes": ["forge.tokens.audited_change_present"]}');
