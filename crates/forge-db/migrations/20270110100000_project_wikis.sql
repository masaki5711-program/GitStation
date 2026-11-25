-- Plan 20 W1 (ADR-0040 §5, option W-C): a project's wiki is an ordinary
-- project in the same namespace, linked by one row here. The owner
-- project's members inherit their role on the wiki — one way, one hop —
-- in the role computation (`forge_db::roles`, `chain_role_columns!`), which
-- reads this table; nothing else decides with it.
--
-- Derived kind in the ledger like forge.org_bindings: the link and the
-- unlink write their own audit rows (`project.wiki_linked`,
-- `project.wiki_unlinked`, on the owner project); the purge of either
-- project removes the row under its `project.purged` row.
--
--   project_id       the owner project (at most one wiki).
--   wiki_project_id  the wiki project (the wiki of at most one project).
--
-- One hop only: a project is never its own wiki (CHECK), a wiki never owns
-- a wiki and an owner is never a wiki (trigger project_wikis_no_chain,
-- serialized by a transaction advisory lock so two concurrent links cannot
-- build a chain between them).
CREATE TABLE forge.project_wikis (
    project_id       bigint PRIMARY KEY REFERENCES forge.projects (id),
    wiki_project_id  bigint NOT NULL REFERENCES forge.projects (id)
        CONSTRAINT project_wikis_wiki_once UNIQUE,
    linked_by        bigint NOT NULL REFERENCES forge.users (id),
    linked_at        timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT project_wikis_not_self CHECK (project_id <> wiki_project_id)
);

CREATE FUNCTION forge.project_wikis_no_chain() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, forge, pg_temp
AS $$
BEGIN
    -- One writer of links at a time (links are rare; a chain needs two rows).
    PERFORM pg_catalog.pg_advisory_xact_lock(pg_catalog.hashtext('forge.project_wikis'));
    IF EXISTS (SELECT 1 FROM forge.project_wikis w
               WHERE w.wiki_project_id = NEW.project_id
                  OR w.project_id = NEW.wiki_project_id) THEN
        RAISE EXCEPTION 'a wiki link is one hop only'
            USING ERRCODE = 'check_violation', CONSTRAINT = 'project_wikis_no_chain';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER project_wikis_no_chain
    BEFORE INSERT OR UPDATE ON forge.project_wikis
    FOR EACH ROW EXECUTE FUNCTION forge.project_wikis_no_chain();

-- The higher of two stored roles (the role computation's max of a wiki's
-- own role and the inherited one). A value outside the five roles is kept,
-- so the role loaders still report it (FG-DB-0018), as forge.org_role_max.
CREATE FUNCTION forge.role_max(a text, b text)
    RETURNS text
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
AS $$
    SELECT CASE
        WHEN a IS NULL THEN b
        WHEN b IS NULL THEN a
        WHEN pg_catalog.array_position(
                 ARRAY['guest', 'reporter', 'developer', 'maintainer', 'owner'], a) IS NULL THEN a
        WHEN pg_catalog.array_position(
                 ARRAY['guest', 'reporter', 'developer', 'maintainer', 'owner'], b) IS NULL THEN b
        WHEN pg_catalog.array_position(
                 ARRAY['guest', 'reporter', 'developer', 'maintainer', 'owner'], b)
           > pg_catalog.array_position(
                 ARRAY['guest', 'reporter', 'developer', 'maintainer', 'owner'], a) THEN b
        ELSE a END
$$;
REVOKE ALL ON FUNCTION forge.role_max(text, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION forge.role_max(text, text) TO forge_app, forge_ro;

GRANT SELECT, INSERT, DELETE ON forge.project_wikis TO forge_app;
GRANT SELECT ON forge.project_wikis TO forge_ro;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20270110100000, "guarded_changes": []}');
