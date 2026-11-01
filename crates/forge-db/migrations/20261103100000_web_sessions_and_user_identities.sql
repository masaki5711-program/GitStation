-- M2 MR3 (ADR-0021 §4, §5, ADR-0018, ADR-0016 §1): web sessions and the
-- links between users and their IdP identities. Both are state tables
-- (tables.toml kind = "audited"): every audited change commits only with a
-- succeeded audit row for the user (resource 'user', id = user_id).
--
-- web_sessions: one row per browser session. The cookie __Host-gstn_session
-- is gss_<public_id>_<secret>; only sha256(secret) is stored (secret_sha256).
-- Issuing (session.created) and revoking (session.revoked, and the user's
-- disable: user.disabled) are audited; the activity columns (last_seen_at,
-- idle_expires_at, at most once a minute) and the refresh check's columns
-- (refresh_token_enc, id_token_enc, refresh_checked_at, access_role_seen_at,
-- ADR-0021 §7) are not. refresh_token_enc / id_token_enc are AES-256-GCM
-- with per-purpose HKDF keys (web.session_key_file), the public_id as
-- associated data. forge_ro never sees the hash, the ciphertexts or the
-- IdP's subject. forge_app may insert and update the unaudited columns and
-- revoked_at / revoked_reason; no DELETE yet (the cleanup of rows 30 days
-- past their absolute expiry gets its own grant with its job). No role
-- gets TRUNCATE.
--
-- user_identities: (issuer, subject) of an OIDC IdP linked to a user
-- (ADR-0021 §4; the login that writes it is MR4). One identity per user and
-- issuer. last_login_at is unaudited. forge_app may insert and update
-- last_login_at only (re-linking is an approved CLI step); forge_ro never
-- sees the subject.

CREATE TABLE forge.web_sessions (
    id                  bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    public_id           text NOT NULL,
    secret_sha256       bytea NOT NULL,
    user_id             bigint NOT NULL REFERENCES forge.users (id),
    created_at          timestamptz NOT NULL DEFAULT now(),
    last_seen_at        timestamptz NOT NULL DEFAULT now(),
    idle_expires_at     timestamptz NOT NULL,
    absolute_expires_at timestamptz NOT NULL,
    revoked_at          timestamptz,
    revoked_reason      text,
    oidc_issuer         text,
    oidc_sid            text,
    oidc_sub            text,
    refresh_token_enc   bytea,
    id_token_enc        bytea,
    refresh_checked_at  timestamptz,
    access_role_seen_at timestamptz,
    CONSTRAINT web_sessions_public_id_key UNIQUE (public_id),
    CONSTRAINT web_sessions_public_id_hex CHECK (public_id ~ '^[0-9a-f]{16}$'),
    CONSTRAINT web_sessions_secret_sha256_len CHECK (octet_length(secret_sha256) = 32),
    -- ADR-0021 §5: the absolute lifetime is at most 24 hours; activity never
    -- extends a session past it.
    CONSTRAINT web_sessions_expiry_range CHECK (
        absolute_expires_at > created_at
        AND absolute_expires_at <= created_at + interval '24 hours'
        AND idle_expires_at <= absolute_expires_at),
    CONSTRAINT web_sessions_revoked_reason CHECK (
        revoked_reason IS NULL
        OR revoked_reason IN ('logout', 'backchannel', 'user_disabled', 'admin', 'idp_revoked')),
    CONSTRAINT web_sessions_revoked_pair CHECK ((revoked_at IS NULL) = (revoked_reason IS NULL)),
    -- An OIDC session names its issuer and subject together (sid is optional).
    CONSTRAINT web_sessions_oidc_pair CHECK ((oidc_issuer IS NULL) = (oidc_sub IS NULL)),
    CONSTRAINT web_sessions_oidc_issuer_length CHECK (char_length(oidc_issuer) BETWEEN 1 AND 255),
    CONSTRAINT web_sessions_oidc_sub_length CHECK (char_length(oidc_sub) BETWEEN 1 AND 255),
    CONSTRAINT web_sessions_oidc_sid_length CHECK (char_length(oidc_sid) BETWEEN 1 AND 255)
);
-- The user's active sessions (a disable revokes them; the list, MR6).
CREATE INDEX web_sessions_user_active ON forge.web_sessions (user_id) WHERE revoked_at IS NULL;
-- The back-channel logout (ADR-0021 §8): by issuer and sid, or issuer and sub.
CREATE INDEX web_sessions_oidc_sid ON forge.web_sessions (oidc_issuer, oidc_sid)
    WHERE revoked_at IS NULL AND oidc_sid IS NOT NULL;
CREATE INDEX web_sessions_oidc_sub ON forge.web_sessions (oidc_issuer, oidc_sub)
    WHERE revoked_at IS NULL AND oidc_sub IS NOT NULL;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, public_id, secret_sha256, user_id, created_at,
        absolute_expires_at, revoked_at, revoked_reason, oidc_issuer, oidc_sid, oidc_sub
    ON forge.web_sessions
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('user', 'user_id',
        'last_seen_at,idle_expires_at,refresh_token_enc,id_token_enc,refresh_checked_at,access_role_seen_at',
        '');
ALTER TABLE forge.web_sessions ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT, INSERT ON forge.web_sessions TO forge_app;
GRANT UPDATE (last_seen_at, idle_expires_at, revoked_at, revoked_reason, refresh_token_enc,
    id_token_enc, refresh_checked_at, access_role_seen_at) ON forge.web_sessions TO forge_app;
GRANT SELECT (id, public_id, user_id, created_at, last_seen_at, idle_expires_at,
    absolute_expires_at, revoked_at, revoked_reason, oidc_issuer, oidc_sid, refresh_checked_at,
    access_role_seen_at) ON forge.web_sessions TO forge_ro;

CREATE TABLE forge.user_identities (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id       bigint NOT NULL REFERENCES forge.users (id),
    issuer        text NOT NULL CONSTRAINT user_identities_issuer_length
        CHECK (char_length(issuer) BETWEEN 1 AND 255),
    subject       text NOT NULL CONSTRAINT user_identities_subject_length
        CHECK (char_length(subject) BETWEEN 1 AND 255),
    linked_at     timestamptz NOT NULL DEFAULT now(),
    last_login_at timestamptz,
    CONSTRAINT user_identities_issuer_subject_key UNIQUE (issuer, subject),
    CONSTRAINT user_identities_user_issuer_key UNIQUE (user_id, issuer)
);

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, user_id, issuer, subject, linked_at
    ON forge.user_identities
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('user', 'user_id', 'last_login_at', '');
ALTER TABLE forge.user_identities ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT, INSERT ON forge.user_identities TO forge_app;
GRANT UPDATE (last_login_at) ON forge.user_identities TO forge_app;
GRANT SELECT (id, user_id, issuer, linked_at, last_login_at) ON forge.user_identities TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.web_sessions'),
    ('trigger', 'audited_change_present on forge.user_identities')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261103100000, "guarded_changes": ["forge.web_sessions.audited_change_present", "forge.user_identities.audited_change_present"]}');
