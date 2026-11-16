-- Plan 17 P17-3 (ADR-0035, plan 17 §0.4): a project's `ai_assets.enabled`,
-- the opt-in of the AI asset lint, the forge/risk rules `ai_case_raw_data`
-- (stop) / `ai_asset` (warn) and (P17-7) asset releases.
--
-- DERIVED state (tables.toml kind = "derived"), as forge.project_auto_merge:
-- what a PERSON does — turn it on or off (Maintainer or above, a web
-- session, never an agent, a bot, a service account or the auto-merge
-- principal) — is audited by the command in the same transaction
-- (forge_db::ai_assets, ChangeTx: `ai_assets.enabled` / `.disabled`,
-- resource the project).
--
-- A project WITHOUT a row is off (every project now, and every new one):
-- nothing changes for a project until a person turns it on.
--   enabled               on / off
--   changed_by / _at      who changed it last, and when
--   version               1, +1 on every change: part of the dedup key of
--                         the `check.risk` jobs a change asks for
--                         (`risk:<project>:<head>:mr<id>:ai<version>`), so
--                         a head assessed before the change is assessed
--                         again with the rules now in force
-- Turning it off removes nothing: history, releases, signatures and
-- deployment records stay (plan 17 §0.4).

CREATE TABLE forge.project_ai_assets (
    project_id  bigint PRIMARY KEY REFERENCES forge.projects (id),
    enabled     boolean NOT NULL,
    changed_by  bigint NOT NULL REFERENCES forge.users (id),
    changed_at  timestamptz NOT NULL DEFAULT now(),
    version     bigint NOT NULL DEFAULT 1 CONSTRAINT project_ai_assets_version CHECK (version >= 1)
);
CREATE INDEX project_ai_assets_changed_by ON forge.project_ai_assets (changed_by);

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.project_ai_assets TO forge_app;
GRANT SELECT ON forge.project_ai_assets TO forge_ro;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261222100000, "guarded_changes": []}');
