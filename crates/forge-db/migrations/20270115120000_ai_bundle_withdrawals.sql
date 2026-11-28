-- Plan 17 P17-7b (ADR-0031 Amendment P17-7b): the withdrawal of a bad
-- `pfs-ai-bundle` release.
--
-- forge.ai_bundle_withdrawals (audited on ('project', project_id)): a
-- Maintainer marked the release `release_id` withdrawn, with a reason. A
-- release row never changes (forge.ai_bundle_releases_guard), so the mark
-- is a row of its own: at most one per release (a withdrawal is never
-- repeated), never changed or deleted (the guard: a withdrawal is never
-- undone; a fixed bundle is a new release). A withdrawn release is never
-- requested on a stage, approved, run by the worker or gone back to (the
-- manual rollback and the worker's automatic one). The withdrawal writes
-- its audit row (`ai_asset_release.withdrawn`) in the same transaction.

CREATE TABLE forge.ai_bundle_withdrawals (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    release_id    bigint NOT NULL REFERENCES forge.ai_bundle_releases (id),
    project_id    bigint NOT NULL,
    reason        text NOT NULL CONSTRAINT ai_bundle_withdrawals_reason
        CHECK (char_length(reason) BETWEEN 1 AND 500 AND reason !~ '[[:cntrl:]]'
               AND btrim(reason) <> ''),
    withdrawn_by  bigint NOT NULL REFERENCES forge.users (id),
    withdrawn_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT ai_bundle_withdrawals_once UNIQUE (release_id)
);
CREATE INDEX ai_bundle_withdrawals_project ON forge.ai_bundle_withdrawals (project_id);
CREATE INDEX ai_bundle_withdrawals_withdrawn_by ON forge.ai_bundle_withdrawals (withdrawn_by);

CREATE FUNCTION forge.ai_bundle_withdrawals_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION USING ERRCODE = 'XF001',
            MESSAGE = 'a withdrawal of forge.ai_bundle_withdrawals is never undone';
    END IF;
    RAISE EXCEPTION USING ERRCODE = 'XF001',
        MESSAGE = 'a withdrawal of forge.ai_bundle_withdrawals never changes';
END
$$;
REVOKE ALL ON FUNCTION forge.ai_bundle_withdrawals_guard() FROM PUBLIC;
CREATE TRIGGER ai_bundle_withdrawals_forward BEFORE UPDATE OR DELETE ON forge.ai_bundle_withdrawals
    FOR EACH ROW EXECUTE FUNCTION forge.ai_bundle_withdrawals_guard();
ALTER TABLE forge.ai_bundle_withdrawals ENABLE ALWAYS TRIGGER ai_bundle_withdrawals_forward;

CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF id, release_id, project_id, reason, withdrawn_by,
        withdrawn_at
    ON forge.ai_bundle_withdrawals
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('project', 'project_id', '', '');
ALTER TABLE forge.ai_bundle_withdrawals ENABLE ALWAYS TRIGGER audited_change_present;

GRANT SELECT ON forge.ai_bundle_withdrawals TO forge_app;
GRANT INSERT (release_id, project_id, reason, withdrawn_by) ON forge.ai_bundle_withdrawals
    TO forge_app;
GRANT SELECT ON forge.ai_bundle_withdrawals TO forge_ro;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('function', 'forge.ai_bundle_withdrawals_guard()'),
    ('trigger', 'ai_bundle_withdrawals_forward on forge.ai_bundle_withdrawals'),
    ('trigger', 'audited_change_present on forge.ai_bundle_withdrawals')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20270115120000, "guarded_changes": ["forge.ai_bundle_withdrawals_guard", "forge.ai_bundle_withdrawals.ai_bundle_withdrawals_forward", "forge.ai_bundle_withdrawals.audited_change_present"]}');
