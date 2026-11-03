-- M3 I1b re-review (database MEDIUM): the audit match of the deferred
-- trigger merge_request_work_items_audited (20261116100100, replaced in
-- 20261116100200) accepted any one work_item.* audit row of the project
-- for any number of link rows. A person's link or unlink now needs a
-- work_item.* audit row of the same transaction whose details name the
-- link's work item (`work_item_id`, as WorkItemChange::to_value writes it);
-- a converted end is covered by the person's work_item.linked of the same
-- work item. An unbinding's ends stay covered by the change's
-- integration.unbound row (it ends many links at once). Accident-level,
-- like the C5 derived-event checks (ADR-0016 Amendment I1b): forge_app can
-- write audit rows itself.

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
              AND (v_action = 'integration.unbound'
                   OR a.details ->> 'work_item_id' = NEW.work_item_id::text)
        ) THEN
            RAISE EXCEPTION USING ERRCODE = 'XF002',
                CONSTRAINT = 'merge_request_work_items_audited',
                SCHEMA = TG_TABLE_SCHEMA, TABLE = TG_TABLE_NAME,
                MESSAGE = pg_catalog.format(
                    'a person''s work item link change on forge.%s has no %s audit row naming its work item in its transaction',
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

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261116100300, "guarded_changes": ["forge.merge_request_work_items_audited"]}');
