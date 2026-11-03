-- M3 I1b (plan 11 §5 I1, plan 02 §2 / §5.7 / §5.8 / §8.2, ADR-0016 §1 / §7,
-- ADR-0016 Amendment I1b): work items and their links. All three tables are
-- derived: the ordered subscriber work-item-sync writes them from the
-- outbox (it has no audit row; DerivedTx), and a person's manual link or
-- unlink writes merge_request_work_items with its own work_item.* audit row
-- and event through ChangeTx.
--
-- 1. work_items: one kanflow work item, identified by (integration,
--    external project, ref) (02 §2: PBI numbers are per kanflow project;
--    which project is the binding's). ref is canonical, `PBI-<n>` without
--    leading zeros (work_items_ref_format; the matcher's output). Never
--    updated or deleted (no grant): links and the event history point at it
--    for good, whatever happens to the binding or the branch.
--
-- 2. merge_request_work_items: one interval of a link between a merge
--    request and a work item. source = auto (detected in the title or the
--    source branch) or manual (a person, linked_by). Ending a link sets
--    unlinked_at / unlink_source (and unlinked_by for a person) once; the
--    row is never deleted (02 §5.7 "リンクは消えない"), except by the project
--    purge. At most one open interval per pair
--    (merge_request_work_items_one_active); a relink is a new row. The guard
--    merge_request_work_items_guard keeps the identity and the link fields
--    fixed, an ended row final, and DELETE for a project pending deletion
--    (the purge) only.
--
-- 3. event_work_items: which work items an event concerns (seq of
--    forge.events), written by work-item-sync for each event it handles —
--    the feed's filter (ADR-0016 §7: the index (work_item, seq)). No FK to
--    forge.events (its rows are removed by retention, C7, which will remove
--    these with them); no UPDATE, no DELETE yet.

CREATE TABLE forge.work_items (
    id                   bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    integration_id       bigint NOT NULL REFERENCES forge.integrations (id),
    external_project_id  text NOT NULL
        CONSTRAINT work_items_external_project_id_format
        CHECK (external_project_id ~ '^[A-Za-z0-9._:-]{1,100}$'),
    ref                  text COLLATE "C" NOT NULL
        CONSTRAINT work_items_ref_format CHECK (ref ~ '^PBI-(0|[1-9][0-9]{0,6})$'),
    created_at           timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT work_items_identity UNIQUE (integration_id, external_project_id, ref)
);

CREATE TABLE forge.merge_request_work_items (
    id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    merge_request_id  bigint NOT NULL REFERENCES forge.merge_requests (id),
    project_id        bigint NOT NULL REFERENCES forge.projects (id),
    work_item_id      bigint NOT NULL REFERENCES forge.work_items (id),
    source            text NOT NULL CONSTRAINT merge_request_work_items_source
        CHECK (source IN ('auto', 'manual')),
    linked_by         bigint REFERENCES forge.users (id),
    -- The event whose handling made an auto link (NULL for a manual one).
    linked_seq        bigint CONSTRAINT merge_request_work_items_linked_seq
        CHECK (linked_seq > 0),
    linked_at         timestamptz NOT NULL DEFAULT now(),
    unlinked_at       timestamptz,
    unlink_source     text CONSTRAINT merge_request_work_items_unlink_source
        CHECK (unlink_source IN ('auto', 'manual')),
    unlinked_by       bigint REFERENCES forge.users (id),
    CONSTRAINT merge_request_work_items_source_fields CHECK (
        (source = 'auto' AND linked_by IS NULL AND linked_seq IS NOT NULL)
        OR (source = 'manual' AND linked_by IS NOT NULL AND linked_seq IS NULL)),
    CONSTRAINT merge_request_work_items_unlink_source_fields CHECK (
        (unlinked_at IS NULL AND unlink_source IS NULL AND unlinked_by IS NULL)
        OR (unlinked_at IS NOT NULL AND unlink_source = 'auto' AND unlinked_by IS NULL)
        OR (unlinked_at IS NOT NULL AND unlink_source = 'manual' AND unlinked_by IS NOT NULL))
);
CREATE UNIQUE INDEX merge_request_work_items_one_active
    ON forge.merge_request_work_items (merge_request_id, work_item_id)
    WHERE unlinked_at IS NULL;
-- The merge request's links (and their history); the project purge.
CREATE INDEX merge_request_work_items_merge_request
    ON forge.merge_request_work_items (merge_request_id, id);
CREATE INDEX merge_request_work_items_project
    ON forge.merge_request_work_items (project_id);
-- A work item's merge requests (the development status, I1d).
CREATE INDEX merge_request_work_items_work_item
    ON forge.merge_request_work_items (work_item_id);

CREATE FUNCTION forge.merge_request_work_items_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        IF NOT EXISTS (SELECT 1 FROM forge.projects AS p
                       WHERE p.id = OLD.project_id AND p.deleted_at IS NOT NULL) THEN
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
    RETURN NEW;
END
$$;
REVOKE ALL ON FUNCTION forge.merge_request_work_items_guard() FROM PUBLIC;
CREATE TRIGGER merge_request_work_items_end_once BEFORE UPDATE OR DELETE
    ON forge.merge_request_work_items
    FOR EACH ROW EXECUTE FUNCTION forge.merge_request_work_items_guard();
ALTER TABLE forge.merge_request_work_items ENABLE ALWAYS TRIGGER merge_request_work_items_end_once;

CREATE TABLE forge.event_work_items (
    seq           bigint NOT NULL CONSTRAINT event_work_items_seq_positive CHECK (seq > 0),
    work_item_id  bigint NOT NULL REFERENCES forge.work_items (id),
    PRIMARY KEY (seq, work_item_id)
);
CREATE INDEX event_work_items_work_item_seq ON forge.event_work_items (work_item_id, seq);

GRANT SELECT, INSERT ON forge.work_items TO forge_app;
GRANT SELECT ON forge.work_items TO forge_ro;
GRANT SELECT, INSERT, DELETE ON forge.merge_request_work_items TO forge_app;
GRANT UPDATE (unlinked_at, unlink_source, unlinked_by) ON forge.merge_request_work_items TO forge_app;
GRANT SELECT ON forge.merge_request_work_items TO forge_ro;
GRANT SELECT, INSERT ON forge.event_work_items TO forge_app;
GRANT SELECT ON forge.event_work_items TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('function', 'forge.merge_request_work_items_guard()'),
    ('trigger', 'merge_request_work_items_end_once on forge.merge_request_work_items')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261116100000, "guarded_changes": ["forge.merge_request_work_items_guard", "forge.merge_request_work_items.merge_request_work_items_end_once"]}');
