-- M4 C13 (ADR-0033 §2, plan 14 C13): people's SSH public keys for the SSH
-- git transport.
--
-- forge.user_ssh_keys (tables.toml kind = "audited", on ('user', user_id)):
-- added by the person in a web session (`POST /api/v1/user/ssh-keys`,
-- `ssh_key.added`) and deleted by them (`DELETE …/{id}`,
-- `ssh_key.deleted`); audit only, no event. The SSH server looks a key up
-- by its SHA-256 fingerprint (`SHA256:<43 base64 characters>`, what
-- `ssh-keygen -l -E sha256` prints); a key identifies one person, so the
-- fingerprint is unique across all users.
--
--   title         the person's label (1..=100 characters; the key's
--                 comment when they gave none)
--   algorithm     ssh-ed25519 / ecdsa-sha2-nistp256|384|521 (RSA, DSA,
--                 security-key and certificate types are refused)
--   public_key    the canonical `<algorithm> <base64>` line (no comment);
--                 public data, shown back to its owner
--   last_used_at  refreshed by authentication at most every 10 minutes
--                 (unaudited, like tokens.last_used_at)
--
-- A disabled owner's keys stay but authenticate nobody (the lookup joins
-- the owner's state). At most 50 keys per user (forge-db, under the
-- owner's row lock).

CREATE TABLE forge.user_ssh_keys (
    id                  bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id             bigint NOT NULL REFERENCES forge.users (id),
    title               text NOT NULL,
    algorithm           text NOT NULL,
    public_key          text NOT NULL,
    fingerprint_sha256  text NOT NULL,
    created_at          timestamptz NOT NULL DEFAULT now(),
    last_used_at        timestamptz,
    CONSTRAINT user_ssh_keys_fingerprint_key UNIQUE (fingerprint_sha256),
    CONSTRAINT user_ssh_keys_fingerprint_shape CHECK (
        fingerprint_sha256 ~ '^SHA256:[A-Za-z0-9+/]{43}$'),
    CONSTRAINT user_ssh_keys_title_length CHECK (char_length(title) BETWEEN 1 AND 100),
    CONSTRAINT user_ssh_keys_algorithm_known CHECK (algorithm IN (
        'ssh-ed25519', 'ecdsa-sha2-nistp256', 'ecdsa-sha2-nistp384', 'ecdsa-sha2-nistp521')),
    CONSTRAINT user_ssh_keys_public_key_shape CHECK (
        public_key LIKE algorithm || ' %' AND octet_length(public_key) <= 1024)
);
CREATE INDEX user_ssh_keys_user ON forge.user_ssh_keys (user_id);

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, user_id, title, algorithm, public_key,
        fingerprint_sha256, created_at
    ON forge.user_ssh_keys
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('user', 'user_id', 'last_used_at', '');
ALTER TABLE forge.user_ssh_keys ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT, INSERT, DELETE ON forge.user_ssh_keys TO forge_app;
GRANT UPDATE (last_used_at) ON forge.user_ssh_keys TO forge_app;
GRANT SELECT ON forge.user_ssh_keys TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('trigger', 'audited_change_present on forge.user_ssh_keys')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261218100000, "guarded_changes": ["forge.user_ssh_keys.audited_change_present"]}');
