-- Step C2, review round 4 (ADR-0018 §3, amendment): only a migration
-- (forge_owner) writes db.migrate audit rows. Replaces forge.audit_log_guard();
-- the rest of the function is unchanged.

CREATE OR REPLACE FUNCTION forge.audit_log_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    high bigint;
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.seq IS NOT NULL OR NEW.sequenced_at IS NOT NULL THEN
            RAISE EXCEPTION USING ERRCODE = 'XF001',
                MESSAGE = 'INSERT on forge.audit_log with seq is not allowed (assigned by the sequencer)';
        END IF;
        -- The application never writes the rows that the migrations and the
        -- privileged functions write about themselves.
        IF current_user = 'forge_app'
           AND (NEW.action = 'db.migrate' OR NEW.action LIKE 'audit.%'
                OR NEW.action LIKE 'feed.%' OR NEW.action LIKE 'events.%') THEN
            RAISE EXCEPTION USING ERRCODE = 'XF001',
                MESSAGE = pg_catalog.format('audit action %s is reserved', NEW.action);
        END IF;
        -- A migration record comes from a migration (forge_owner) only: one
        -- written by any other role could never be removed and would turn
        -- doctor's migration_records check red for good.
        IF NEW.action = 'db.migrate' AND current_user <> 'forge_owner' THEN
            RAISE EXCEPTION USING ERRCODE = 'XF001',
                MESSAGE = 'audit action db.migrate is written by migrations only';
        END IF;
        NEW.txid := pg_catalog.pg_current_xact_id();
        NEW.occurred_at := pg_catalog.now();
        RETURN NEW;
    ELSIF TG_OP = 'UPDATE' THEN
        IF current_user = 'forge_seq' AND OLD.seq IS NULL AND NEW.seq IS NOT NULL
           AND (pg_catalog.to_jsonb(NEW) - 'seq'::text - 'sequenced_at'::text)
               = (pg_catalog.to_jsonb(OLD) - 'seq'::text - 'sequenced_at'::text) THEN
            SELECT s.audit_high_seq INTO high FROM ONLY forge.event_feed_state AS s WHERE s.singleton;
            IF high IS NULL OR NEW.seq <= high OR NEW.seq > high + 1000 THEN
                RAISE EXCEPTION USING ERRCODE = 'XF001',
                    MESSAGE = pg_catalog.format(
                        'seq %s on forge.audit_log is outside the sequencing window (audit_high_seq %s)',
                        NEW.seq, coalesce(high::text, 'missing'));
            END IF;
            NEW.sequenced_at := pg_catalog.now();
            RETURN NEW;
        -- Redaction: all personal columns to NULL at once, nothing else (the
        -- per-seal-version check comes with the seals, C8).
        ELSIF current_user = 'forge_purge'
              AND NEW.actor_label IS NULL AND NEW.source_ip IS NULL AND NEW.user_agent IS NULL
              AND (pg_catalog.to_jsonb(NEW) - 'actor_label'::text - 'source_ip'::text
                   - 'user_agent'::text)
                  = (pg_catalog.to_jsonb(OLD) - 'actor_label'::text - 'source_ip'::text
                     - 'user_agent'::text) THEN
            RETURN NEW;
        END IF;
    ELSIF TG_OP = 'DELETE' THEN
        -- Purge only: sequenced rows past the 365-day floor, never the
        -- migration records (forge_audit.ddl_audit_status() matches DDL to
        -- them).
        IF current_user = 'forge_purge' AND OLD.seq IS NOT NULL
           AND OLD.action <> 'db.migrate'
           AND OLD.occurred_at < pg_catalog.now() - interval '365 days' THEN
            RETURN OLD;
        END IF;
    END IF;
    RAISE EXCEPTION USING ERRCODE = 'XF001',
        MESSAGE = pg_catalog.format('%s on forge.audit_log is not allowed (append-only)', TG_OP);
END
$$;
REVOKE ALL ON FUNCTION forge.audit_log_guard() FROM PUBLIC;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261027100300, "guarded_changes": ["forge.audit_log_guard"]}');
