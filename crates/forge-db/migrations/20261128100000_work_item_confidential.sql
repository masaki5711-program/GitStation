-- M3 X5 (plan 06 D-5, plan 02 §12, ADR-0023 Amendment X5): kanflow marks a
-- work item confidential, and the context API never hands a confidential
-- work item to an AI agent (a token issued with is_agent, a service account
-- or a bot). This column holds the mark forge knows of; false until set.
--
-- Nothing in forge sets it yet: no grant changes here (forge_app still has
-- no UPDATE on forge.work_items). The writer — kanflow's own service account
-- sending the mark (plan 02 §12) — arrives with the kanflow side of the
-- integration and grants UPDATE (confidential) then, with its audit row.
-- forge_ro reads it like every other column (tables.toml ro_columns "*").
--
-- Locking: ADD COLUMN with a constant default is a catalog change only (no
-- table rewrite since PostgreSQL 11); it takes an ACCESS EXCLUSIVE lock on
-- forge.work_items for that moment. The table is small (one row per kanflow
-- reference ever named).

ALTER TABLE forge.work_items
    ADD COLUMN confidential boolean NOT NULL DEFAULT false;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261128100000, "guarded_changes": []}');
