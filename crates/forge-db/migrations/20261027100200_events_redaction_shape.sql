-- Step C2, review round 3 (ADR-0018 §3, amendment): event redaction may only
-- blank values. A forge_purge change to a published event keeps the same
-- top-level keys, adds "redacted": true, and replaces values with
-- "[redacted]" or leaves them — nothing new can be written into an event.
-- Replaces forge.events_guard(); the rest of the function is unchanged.

CREATE OR REPLACE FUNCTION forge.events_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    high bigint;
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.seq IS NOT NULL OR NEW.sequenced_at IS NOT NULL THEN
            RAISE EXCEPTION USING ERRCODE = 'XF001',
                MESSAGE = 'INSERT on forge.events with seq is not allowed (assigned by the sequencer)';
        END IF;
        NEW.txid := pg_catalog.pg_current_xact_id();
        NEW.occurred_at := pg_catalog.now();
        RETURN NEW;
    ELSIF TG_OP = 'UPDATE' THEN
        IF current_user = 'forge_seq' AND OLD.seq IS NULL AND NEW.seq IS NOT NULL
           AND (pg_catalog.to_jsonb(NEW) - 'seq'::text - 'sequenced_at'::text)
               = (pg_catalog.to_jsonb(OLD) - 'seq'::text - 'sequenced_at'::text) THEN
            SELECT s.high_seq INTO high FROM ONLY forge.event_feed_state AS s WHERE s.singleton;
            IF high IS NULL OR NEW.seq <= high OR NEW.seq > high + 1000 THEN
                RAISE EXCEPTION USING ERRCODE = 'XF001',
                    MESSAGE = pg_catalog.format(
                        'seq %s on forge.events is outside the sequencing window (high_seq %s)',
                        NEW.seq, coalesce(high::text, 'missing'));
            END IF;
            NEW.sequenced_at := pg_catalog.now();
            RETURN NEW;
        -- Redaction of a published event (events_redact_user, C10).
        ELSIF current_user = 'forge_purge' AND OLD.seq IS NOT NULL
              AND (NEW.payload -> 'redacted'::text) = 'true'::jsonb
              AND NOT EXISTS (
                  SELECT 1 FROM pg_catalog.jsonb_each(NEW.payload) AS kv
                  WHERE kv.key <> 'redacted'
                    AND (NOT (OLD.payload ? kv.key)
                         OR ((kv.value <> (OLD.payload -> kv.key))
                             AND kv.value <> '"[redacted]"'::jsonb)))
              AND NOT EXISTS (
                  SELECT 1 FROM pg_catalog.jsonb_object_keys(OLD.payload) AS k
                  WHERE NOT (NEW.payload ? k))
              AND (pg_catalog.to_jsonb(NEW) - 'payload'::text)
                  = (pg_catalog.to_jsonb(OLD) - 'payload'::text) THEN
            RETURN NEW;
        END IF;
    ELSIF TG_OP = 'DELETE' THEN
        -- Retention only: sequenced (published) rows past the 7-day floor.
        IF current_user = 'forge_purge' AND OLD.seq IS NOT NULL
           AND OLD.occurred_at < pg_catalog.now() - interval '7 days' THEN
            RETURN OLD;
        END IF;
    END IF;
    RAISE EXCEPTION USING ERRCODE = 'XF001',
        MESSAGE = pg_catalog.format('%s on forge.events is not allowed (append-only)', TG_OP);
END
$$;
REVOKE ALL ON FUNCTION forge.events_guard() FROM PUBLIC;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261027100200, "guarded_changes": ["forge.events_guard"]}');
