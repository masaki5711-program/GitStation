-- Step C8 (ADR-0018 §4, the audit_seal row of §3, §1): the audit seal.
-- forge.audit_seal holds one row per sealed range of forge.audit_log: the
-- range [first_seq, last_seq], the SHA-256 of its rows (hash version 1,
-- forge-db seal::v1), the chain to the previous seal and an HMAC whose key
-- lives outside the database (audit.seal_keys_file). The `audit.seal` job of
-- forge worker writes the rows (forge_app); the sink (C9) sets shipped_at.
--
-- The guard keeps every seal as written: only shipped_at may change, once,
-- from NULL to a value; nothing is deleted or truncated. On insert it forces
-- sealed_at, refuses a pre-set shipped_at, keeps forge_app from writing
-- imported seals (forge.audit_seal_resync, C9), and requires the chain: the
-- range starts right after the latest seal (or at 1), prev_seal_sha256 is the
-- latest seal's seal_sha256 (or sha256('forge-audit-genesis')), and a normal
-- or restore seal ends at or below audit_high_seq (rows that have a number).
-- The HMAC cannot be checked here (the key is not in the database).

CREATE TABLE forge.audit_seal (
    id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    kind             text NOT NULL CHECK (kind IN ('normal', 'imported', 'restore')),
    hash_version     smallint NOT NULL CHECK (hash_version >= 1),
    seal_epoch       smallint NOT NULL CHECK (seal_epoch >= 1),
    key_id           smallint NOT NULL CHECK (key_id >= 1),
    first_seq        bigint NOT NULL UNIQUE CHECK (first_seq >= 1),
    last_seq         bigint NOT NULL UNIQUE,
    row_count        bigint NOT NULL,
    events_high_seq  bigint NOT NULL CHECK (events_high_seq >= 0),
    rows_sha256      bytea NOT NULL CHECK (pg_catalog.octet_length(rows_sha256) = 32),
    prev_seal_sha256 bytea NOT NULL CHECK (pg_catalog.octet_length(prev_seal_sha256) = 32),
    seal_sha256      bytea NOT NULL CHECK (pg_catalog.octet_length(seal_sha256) = 32),
    hmac             bytea NOT NULL CHECK (pg_catalog.octet_length(hmac) = 32),
    sealed_at        timestamptz NOT NULL DEFAULT pg_catalog.now(),
    shipped_at       timestamptz,
    CONSTRAINT audit_seal_range CHECK (last_seq >= first_seq),
    CONSTRAINT audit_seal_row_count CHECK (row_count = last_seq - first_seq + 1)
);
-- forge doctor audit.seal_verify reads the seals of the last 24 hours.
CREATE INDEX audit_seal_sealed_at ON forge.audit_seal (sealed_at);

CREATE FUNCTION forge.audit_seal_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    prev_last bigint;
    prev_hash bytea;
    audit_high bigint;
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.shipped_at IS NOT NULL THEN
            RAISE EXCEPTION USING ERRCODE = 'XF001',
                MESSAGE = 'INSERT on forge.audit_seal with shipped_at is not allowed';
        END IF;
        IF current_user = 'forge_app' AND NEW.kind = 'imported' THEN
            RAISE EXCEPTION USING ERRCODE = 'XF001',
                MESSAGE = 'imported seals are written by forge.audit_seal_resync only';
        END IF;
        SELECT s.last_seq, s.seal_sha256 INTO prev_last, prev_hash
          FROM ONLY forge.audit_seal AS s
         ORDER BY s.last_seq DESC
         LIMIT 1;
        IF NEW.first_seq <> coalesce(prev_last, 0) + 1
           OR NEW.prev_seal_sha256
              <> coalesce(prev_hash, pg_catalog.sha256('forge-audit-genesis'::bytea)) THEN
            RAISE EXCEPTION USING ERRCODE = 'XF001',
                MESSAGE = pg_catalog.format(
                    'seal %s..%s on forge.audit_seal does not continue the chain (last sealed seq %s)',
                    NEW.first_seq, NEW.last_seq, coalesce(prev_last::text, 'none'));
        END IF;
        IF NEW.kind <> 'imported' THEN
            SELECT s.audit_high_seq INTO audit_high
              FROM ONLY forge.event_feed_state AS s WHERE s.singleton;
            IF audit_high IS NULL OR NEW.last_seq > audit_high THEN
                RAISE EXCEPTION USING ERRCODE = 'XF001',
                    MESSAGE = pg_catalog.format(
                        'seal %s..%s on forge.audit_seal ends past audit_high_seq %s',
                        NEW.first_seq, NEW.last_seq, coalesce(audit_high::text, 'missing'));
            END IF;
        END IF;
        NEW.sealed_at := pg_catalog.now();
        RETURN NEW;
    ELSIF TG_OP = 'UPDATE' THEN
        IF OLD.shipped_at IS NULL AND NEW.shipped_at IS NOT NULL
           AND (pg_catalog.to_jsonb(NEW) - 'shipped_at'::text)
               = (pg_catalog.to_jsonb(OLD) - 'shipped_at'::text) THEN
            RETURN NEW;
        END IF;
    END IF;
    RAISE EXCEPTION USING ERRCODE = 'XF001',
        MESSAGE = pg_catalog.format('%s on forge.audit_seal is not allowed (append-only)', TG_OP);
END
$$;
REVOKE ALL ON FUNCTION forge.audit_seal_guard() FROM PUBLIC;

CREATE TRIGGER audit_seal_append_only BEFORE INSERT OR UPDATE OR DELETE ON forge.audit_seal
    FOR EACH ROW EXECUTE FUNCTION forge.audit_seal_guard();
CREATE TRIGGER audit_seal_no_truncate BEFORE TRUNCATE ON forge.audit_seal
    FOR EACH STATEMENT EXECUTE FUNCTION forge.audit_seal_guard();
-- ALWAYS: also under session_replication_role = replica.
ALTER TABLE forge.audit_seal ENABLE ALWAYS TRIGGER audit_seal_append_only;
ALTER TABLE forge.audit_seal ENABLE ALWAYS TRIGGER audit_seal_no_truncate;

-- ── Privileges (ADR-0018 §1) ───────────────────────────────────────────
-- forge_app (the audit.seal job, and the sink's shipped_at in C9) and
-- forge_seq (forge.audit_seal_resync, C9) insert every column but id,
-- sealed_at and shipped_at; forge_purge reads the ranges (C10); the read
-- pool reads everything (no secret: the HMAC key is not here).
GRANT SELECT, INSERT (kind, hash_version, seal_epoch, key_id, first_seq, last_seq, row_count,
    events_high_seq, rows_sha256, prev_seal_sha256, seal_sha256, hmac),
    UPDATE (shipped_at) ON forge.audit_seal TO forge_app;
GRANT SELECT, INSERT (kind, hash_version, seal_epoch, key_id, first_seq, last_seq, row_count,
    events_high_seq, rows_sha256, prev_seal_sha256, seal_sha256, hmac)
    ON forge.audit_seal TO forge_seq;
GRANT SELECT ON forge.audit_seal TO forge_purge, forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('table', 'forge.audit_seal'),
    ('function', 'forge.audit_seal_guard()'),
    ('trigger', 'audit_seal_append_only on forge.audit_seal'),
    ('trigger', 'audit_seal_no_truncate on forge.audit_seal')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261027130000, "guarded_changes": ["forge.audit_seal", "forge.audit_seal_guard", "forge.audit_seal.audit_seal_append_only", "forge.audit_seal.audit_seal_no_truncate"]}');
