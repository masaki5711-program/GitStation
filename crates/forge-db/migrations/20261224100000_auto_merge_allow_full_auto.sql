-- M4 close (plan 14 C9 / M4 close; ADR-0017 Amendments 2026-10-30 B,
-- 2026-11-03 B and 2026-11-17): the instance's `merge.allow_full_auto`.
--
-- forge.auto_merge_instance (one row, id 1; DERIVED, tables.toml) gains:
--   allow_full_auto       whether a project may select `full` and whether
--                         `full` acts. Default true: every project created
--                         after C9 is `full` (the 2026-11-03 user decision)
--                         and keeps merging by itself after this migration.
--                         While false, choosing `full` is FG-REVIEW-0032 and
--                         a project already `full` is `reserve` in effect
--                         (its merge requests show held
--                         reason=full_auto_not_allowed; the auto-merge
--                         principal is refused, its queue entries leave).
--   allow_full_auto_by / allow_full_auto_at
--                         the administrator who changed it last, and when.
-- A change is audited by the command in the same transaction
-- (forge_db::review::auto_merge::set_full_auto_decided, ChangeTx:
-- `auto_merge.allow_full_auto_changed`, resource the instance).
--
-- Lock impact: ADD COLUMN with a constant default and nullable columns are
-- catalog-only (PostgreSQL 11 and later); the table has one row.

ALTER TABLE forge.auto_merge_instance
    ADD COLUMN allow_full_auto boolean NOT NULL DEFAULT true,
    ADD COLUMN allow_full_auto_by bigint REFERENCES forge.users (id),
    ADD COLUMN allow_full_auto_at timestamptz,
    ADD CONSTRAINT auto_merge_instance_allow_full_auto
        CHECK (allow_full_auto_by IS NULL OR allow_full_auto_at IS NOT NULL);

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261224100000, "guarded_changes": []}');
