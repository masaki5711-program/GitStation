-- ADR-0038 §1 (step A): the copy of the PFS organisation master, written by
-- the worker task `org_master_sync` from the PFS registry-api
-- (`GET /api/v1/org/units`, `GET /api/v1/org/positions`, every plant).
-- It gives the codes of the org claims (forge.user_org_profiles) a level, a
-- parent and names. The codes' shape is never parsed for a hierarchy.
--
-- Both tables are a mirror (tables.toml kind = "derived"): each successful
-- sync replaces them in one transaction (upsert + delete of the rows no
-- longer listed; every row's synced_at is the sync's time) and writes one
-- `org.master_synced` audit row when anything changed. A failed sync writes
-- nothing: the previous copy stays.
--
-- forge.org_units_master   one row per (plant_code, code): level
--                          (division | department | section | team), the
--                          parent's code (same plant), names ja / en / id,
--                          cost center, the head's staff id (personal data:
--                          not on the read pool), valid_from, version
-- forge.org_positions_master one row per (plant_code, code): names, rank
--                          0..=999, sort order, active, version
--
-- The CHECKs repeat the codes' rules of forge-oidc / forge.user_org_profiles
-- (20261230100000) and of forge-integ's parser (the backstop).

CREATE TABLE forge.org_units_master (
    plant_code     text NOT NULL CONSTRAINT org_units_master_plant_code
        CHECK (plant_code ~ '^[A-Z0-9]{1,4}$'),
    code           text NOT NULL CONSTRAINT org_units_master_code
        CHECK (code ~ '^[A-Z0-9][A-Z0-9_-]{0,19}$'),
    level          text NOT NULL CONSTRAINT org_units_master_level
        CHECK (level IN ('division', 'department', 'section', 'team')),
    parent_code    text CONSTRAINT org_units_master_parent_code
        CHECK (parent_code ~ '^[A-Z0-9][A-Z0-9_-]{0,19}$' AND parent_code <> code),
    name_ja        text NOT NULL CONSTRAINT org_units_master_name_ja
        CHECK (char_length(name_ja) BETWEEN 1 AND 200),
    name_en        text CONSTRAINT org_units_master_name_en
        CHECK (char_length(name_en) BETWEEN 1 AND 200),
    name_id        text CONSTRAINT org_units_master_name_id
        CHECK (char_length(name_id) BETWEEN 1 AND 200),
    cost_center    text CONSTRAINT org_units_master_cost_center
        CHECK (char_length(cost_center) BETWEEN 1 AND 64),
    head_staff_id  text CONSTRAINT org_units_master_head_staff_id
        CHECK (head_staff_id ~ '^[A-Za-z0-9._-]{1,64}$'),
    valid_from     date,
    version        integer NOT NULL CONSTRAINT org_units_master_version
        CHECK (version >= 0),
    synced_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (plant_code, code)
);
-- The walk from a unit to its parent stays on the primary key; the walk
-- down (bindings with descendants, ADR-0038 §2) uses this one.
CREATE INDEX org_units_master_parent ON forge.org_units_master (plant_code, parent_code);

CREATE TABLE forge.org_positions_master (
    plant_code     text NOT NULL CONSTRAINT org_positions_master_plant_code
        CHECK (plant_code ~ '^[A-Z0-9]{1,4}$'),
    code           text NOT NULL CONSTRAINT org_positions_master_code
        CHECK (code ~ '^[a-z][a-z0-9_]{0,63}$'),
    name_ja        text NOT NULL CONSTRAINT org_positions_master_name_ja
        CHECK (char_length(name_ja) BETWEEN 1 AND 200),
    name_en        text CONSTRAINT org_positions_master_name_en
        CHECK (char_length(name_en) BETWEEN 1 AND 200),
    name_id        text CONSTRAINT org_positions_master_name_id
        CHECK (char_length(name_id) BETWEEN 1 AND 200),
    rank           integer NOT NULL CONSTRAINT org_positions_master_rank
        CHECK (rank BETWEEN 0 AND 999),
    sort_order     integer NOT NULL,
    active         boolean NOT NULL,
    version        integer NOT NULL CONSTRAINT org_positions_master_version
        CHECK (version >= 0),
    synced_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (plant_code, code)
);

GRANT SELECT, INSERT, UPDATE, DELETE ON forge.org_units_master TO forge_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON forge.org_positions_master TO forge_app;
-- The read pool: names, levels and the hierarchy (shown next to people's
-- names); never the head's staff id (personal data) or the cost center.
GRANT SELECT (plant_code, code, level, parent_code, name_ja, name_en, name_id, valid_from,
              version, synced_at)
    ON forge.org_units_master TO forge_ro;
GRANT SELECT ON forge.org_positions_master TO forge_ro;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261231100000, "guarded_changes": []}');
