-- M3 I1b review (database HIGH / MEDIUM / LOW): backstops of
-- forge.merge_request_work_items, which is a derived table (the ordered
-- subscriber work-item-sync writes it without an audit row) but also holds
-- a person's links, which must carry their work_item.* audit row.
--
-- 1. The deferred constraint trigger merge_request_work_items_audited
--    (ENABLE ALWAYS): in the transaction that writes them,
--    - a person's link (INSERT source = 'manual') needs a succeeded
--      `work_item.linked` audit row on ('project', project_id), and its end
--      (unlink_source = 'manual') a `work_item.unlinked` one
--      (CONSTRAINT merge_request_work_items_audited, XF002 = FG-DB-0013);
--    - a detected link and its end (source / unlink_source = 'auto') need
--      the work-item-sync subscriber to have advanced in the same
--      transaction (event_subscribers.last_txid, the C5 derived-event rule;
--      CONSTRAINT merge_request_work_items_derived).
-- 2. The guard merge_request_work_items_guard (replaced): a person's link
--    ends only as a person's (unlink_source = 'manual'); DELETE only while
--    the project's repository is `purging` (the purge's own marker,
--    20261102100200), no longer for any project pending deletion.
-- 3. The link's project is its merge request's: the composite foreign key
--    merge_request_work_items_merge_request_project on (merge_request_id,
--    project_id) (on merge_requests_id_project, unique since 20261109100100).
-- 4. linked_at is the database's: forge_app's INSERT is column-level.

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
       OR NEW.linked_seq IS DISTINCT FROM OLD.linked_seq
       OR NEW.linked_at IS DISTINCT FROM OLD.linked_at THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'the link of a row on forge.merge_request_work_items never changes';
    END IF;
    IF OLD.unlinked_at IS NOT NULL AND NEW IS DISTINCT FROM OLD THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'an ended link on forge.merge_request_work_items never changes';
    END IF;
    IF NEW.source = 'manual' AND NEW.unlink_source IS DISTINCT FROM OLD.unlink_source
       AND NEW.unlink_source IS DISTINCT FROM 'manual' THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a person''s link on forge.merge_request_work_items is ended only by a person';
    END IF;
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.merge_request_work_items_guard() FROM PUBLIC;

CREATE FUNCTION forge.merge_request_work_items_audited() RETURNS trigger
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
        v_action := 'work_item.unlinked';
    ELSE
        RETURN NULL;
    END IF;
    IF v_kind = 'manual' THEN
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
CREATE CONSTRAINT TRIGGER merge_request_work_items_audited
    AFTER INSERT OR UPDATE OF unlinked_at ON forge.merge_request_work_items
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.merge_request_work_items_audited();
ALTER TABLE forge.merge_request_work_items ENABLE ALWAYS TRIGGER merge_request_work_items_audited;

ALTER TABLE forge.merge_request_work_items
    ADD CONSTRAINT merge_request_work_items_merge_request_project
    FOREIGN KEY (merge_request_id, project_id) REFERENCES forge.merge_requests (id, project_id);

REVOKE INSERT ON forge.merge_request_work_items FROM forge_app;
GRANT INSERT (merge_request_id, project_id, work_item_id, source, linked_by, linked_seq)
    ON forge.merge_request_work_items TO forge_app;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('function', 'forge.merge_request_work_items_audited()'),
    ('trigger', 'merge_request_work_items_audited on forge.merge_request_work_items')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261116100100, "guarded_changes": ["forge.merge_request_work_items_guard", "forge.merge_request_work_items_audited", "forge.merge_request_work_items.merge_request_work_items_audited", "forge.merge_request_work_items"]}');
