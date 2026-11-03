-- M3 I1b review (security / rust MEDIUM): who links and unlinks by hand,
-- and two more ways a link ends.
--
-- 1. linked_by_kind / unlinked_by_kind: `human`, `agent` (a person's agent
--    token), `service_account`, `bot`. A person's link and every end by
--    hand name the kind; only a human's unlink keeps detection from linking
--    the work item again (work-item-sync reads it).
-- 2. unlink_source gains `converted` (a detected link a person linked by
--    hand: the detected interval ends, the person's begins; audited by the
--    person's work_item.linked) and `unbound` (the project was unbound from
--    that work item's kanflow project; audited by integration.unbound).
--    Both name the person (unlinked_by, unlinked_by_kind).
-- 3. The guard (replaced): the kind is part of the link; a person's link
--    ends only by hand or by the unbinding; only a detected link is
--    converted. The audit trigger (replaced) requires the matching audit
--    row for converted and unbound ends.
-- 4. forge_app's INSERT and UPDATE grants gain the kind columns.
--
-- No backfill: 20261116100000 is unreleased (I1b), so no database holds
-- link rows written without the kinds; the CHECKs validate an empty table.

ALTER TABLE forge.merge_request_work_items
    ADD COLUMN linked_by_kind text CONSTRAINT merge_request_work_items_linked_by_kind
        CHECK (linked_by_kind IN ('human', 'agent', 'service_account', 'bot')),
    ADD COLUMN unlinked_by_kind text CONSTRAINT merge_request_work_items_unlinked_by_kind
        CHECK (unlinked_by_kind IN ('human', 'agent', 'service_account', 'bot'));

ALTER TABLE forge.merge_request_work_items
    DROP CONSTRAINT merge_request_work_items_source_fields,
    ADD CONSTRAINT merge_request_work_items_source_fields CHECK (
        (source = 'auto' AND linked_by IS NULL AND linked_by_kind IS NULL
            AND linked_seq IS NOT NULL)
        OR (source = 'manual' AND linked_by IS NOT NULL AND linked_by_kind IS NOT NULL
            AND linked_seq IS NULL)),
    DROP CONSTRAINT merge_request_work_items_unlink_source,
    ADD CONSTRAINT merge_request_work_items_unlink_source
        CHECK (unlink_source IN ('auto', 'manual', 'converted', 'unbound')),
    DROP CONSTRAINT merge_request_work_items_unlink_source_fields,
    ADD CONSTRAINT merge_request_work_items_unlink_source_fields CHECK (
        (unlinked_at IS NULL AND unlink_source IS NULL AND unlinked_by IS NULL
            AND unlinked_by_kind IS NULL)
        OR (unlinked_at IS NOT NULL AND unlink_source = 'auto' AND unlinked_by IS NULL
            AND unlinked_by_kind IS NULL)
        OR (unlinked_at IS NOT NULL AND unlink_source IN ('manual', 'converted', 'unbound')
            AND unlinked_by IS NOT NULL AND unlinked_by_kind IS NOT NULL));

CREATE OR REPLACE FUNCTION forge.merge_request_work_items_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        IF NOT EXISTS (SELECT 1 FROM forge.repositories AS r
                       WHERE r.project_id = OLD.project_id AND r.state = 'purging') THEN
            RAISE EXCEPTION USING ERRCODE = 'XF001',
                MESSAGE = 'a link on forge.merge_request_work_items is deleted only by the purge of its project';
        END IF;
        RETURN OLD;
    END IF;
    IF NEW.merge_request_id IS DISTINCT FROM OLD.merge_request_id
       OR NEW.project_id IS DISTINCT FROM OLD.project_id
       OR NEW.work_item_id IS DISTINCT FROM OLD.work_item_id
       OR NEW.source IS DISTINCT FROM OLD.source
       OR NEW.linked_by IS DISTINCT FROM OLD.linked_by
       OR NEW.linked_by_kind IS DISTINCT FROM OLD.linked_by_kind
       OR NEW.linked_seq IS DISTINCT FROM OLD.linked_seq
       OR NEW.linked_at IS DISTINCT FROM OLD.linked_at THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the link of a row on forge.merge_request_work_items never changes';
    END IF;
    IF OLD.unlinked_at IS NOT NULL AND NEW IS DISTINCT FROM OLD THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'an ended link on forge.merge_request_work_items never changes';
    END IF;
    -- A person's link ends only by hand or by the unbinding (never as
    -- detection's, never `converted`: only a detected link is converted).
    IF NEW.source = 'manual' AND NEW.unlink_source IS DISTINCT FROM OLD.unlink_source
       AND NEW.unlink_source NOT IN ('manual', 'unbound') THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a person''s link on forge.merge_request_work_items is ended only by hand or by the unbinding';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.merge_request_work_items_guard() FROM PUBLIC;

CREATE OR REPLACE FUNCTION forge.merge_request_work_items_audited() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    v_kind   text;
    v_action text;
BEGIN
    IF TG_OP = 'INSERT' THEN
        v_kind := NEW.source;
        v_action := 'work_item.linked';
    ELSIF OLD.unlinked_at IS NULL AND NEW.unlinked_at IS NOT NULL THEN
        v_kind := NEW.unlink_source;
        v_action := CASE NEW.unlink_source
            WHEN 'converted' THEN 'work_item.linked'
            WHEN 'unbound' THEN 'integration.unbound'
            ELSE 'work_item.unlinked' END;
    ELSE
        RETURN NULL;
    END IF;
    IF v_kind IN ('manual', 'converted', 'unbound') THEN
        IF NOT EXISTS (
            SELECT 1 FROM ONLY forge.audit_log AS a
            WHERE a.txid = pg_catalog.pg_current_xact_id() AND a.outcome = 'succeeded'
              AND a.action = v_action AND a.resource_type = 'project'
              AND a.resource_id = NEW.project_id
        ) THEN
            RAISE EXCEPTION USING ERRCODE = 'XF002',
                CONSTRAINT = 'merge_request_work_items_audited',
                SCHEMA = TG_TABLE_SCHEMA, TABLE = TG_TABLE_NAME,
                MESSAGE = pg_catalog.format(
                    'a person''s work item link change on forge.%s has no %s audit row in its transaction',
                    TG_TABLE_NAME, v_action);
        END IF;
    ELSIF NOT EXISTS (
        SELECT 1 FROM forge.event_subscribers AS s
        WHERE s.name = 'work-item-sync' AND s.last_txid = pg_catalog.pg_current_xact_id()
    ) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF002',
            CONSTRAINT = 'merge_request_work_items_derived',
            SCHEMA = TG_TABLE_SCHEMA, TABLE = TG_TABLE_NAME,
            MESSAGE = pg_catalog.format(
                'a detected work item link change on forge.%s was not written by the work-item-sync subscriber',
                TG_TABLE_NAME);
    END IF;
    RETURN NULL;
END
$$;
REVOKE ALL ON FUNCTION forge.merge_request_work_items_audited() FROM PUBLIC;

REVOKE INSERT, UPDATE ON forge.merge_request_work_items FROM forge_app;
GRANT INSERT (merge_request_id, project_id, work_item_id, source, linked_by, linked_by_kind,
    linked_seq) ON forge.merge_request_work_items TO forge_app;
GRANT UPDATE (unlinked_at, unlink_source, unlinked_by, unlinked_by_kind)
    ON forge.merge_request_work_items TO forge_app;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261116100200, "guarded_changes": ["forge.merge_request_work_items_guard", "forge.merge_request_work_items_audited", "forge.merge_request_work_items"]}');
