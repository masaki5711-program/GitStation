-- Step D1 (ADR-0016 §1): a change to a state table (tables.toml kind =
-- "audited") commits only together with a succeeded audit row of the same
-- transaction for the same resource. Each audited table attaches this
-- function as a deferred constraint trigger with four arguments:
--   0 resource type: a literal, or `=column` to read it from the row
--   1 the column holding the audit row's resource_id
--   2 unaudited columns (comma separated, may be empty): changes to them
--     alone need no audit row
--   3 txn-audited columns (comma separated, may be empty): changes to them
--     alone need a succeeded audit row of the same resource type in the
--     transaction (a path rewrite of a subtree is audited once, not once
--     per row)
-- Every other column in the trigger's UPDATE OF list is audited; a column
-- added later is audited once the trigger is re-created with it (doctor
-- reports guard=audited_trigger until then; a mixed update that also changes
-- an audited column is caught regardless, the whole row is compared). An
-- update that changes the resource (id or =column) needs an audit row for the
-- old and for the new resource. The id column must be an integer column.
-- A write that bypasses the audit fails at COMMIT with XF002 / CONSTRAINT
-- audited_change (FG-DB-0013); a trigger the ledger does not describe (wrong
-- argument count, a non-integer id) fails with CONSTRAINT
-- audited_change_config. Messages name the table only, never row values.

CREATE FUNCTION forge.audited_change_present() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    v_old jsonb;
    v_new jsonb;
    v_skip text[];
    v_txn text[];
    v_rows jsonb[];
    v_r jsonb;
    v_type text;
    v_idtext text;
    v_id bigint;
BEGIN
    IF TG_NARGS <> 4 THEN
        RAISE EXCEPTION USING ERRCODE = 'XF002', CONSTRAINT = 'audited_change_config',
            SCHEMA = TG_TABLE_SCHEMA, TABLE = TG_TABLE_NAME,
            MESSAGE = pg_catalog.format(
                'the audit trigger of forge.%s needs 4 arguments', TG_TABLE_NAME);
    END IF;
    v_skip := ARRAY(SELECT pg_catalog.btrim(c)
                    FROM pg_catalog.unnest(pg_catalog.string_to_array(TG_ARGV[2], ',')) AS c
                    WHERE pg_catalog.btrim(c) <> '');
    v_txn := ARRAY(SELECT pg_catalog.btrim(c)
                   FROM pg_catalog.unnest(pg_catalog.string_to_array(TG_ARGV[3], ',')) AS c
                   WHERE pg_catalog.btrim(c) <> '');
    IF TG_OP = 'DELETE' THEN
        v_rows := ARRAY[pg_catalog.to_jsonb(OLD)];
    ELSIF TG_OP = 'INSERT' THEN
        v_rows := ARRAY[pg_catalog.to_jsonb(NEW)];
    ELSE
        v_rows := ARRAY[pg_catalog.to_jsonb(NEW), pg_catalog.to_jsonb(OLD)];
    END IF;
    -- The columns the ledger names must be in the row (doctor checks this
    -- too); a trigger that names another column is a schema defect.
    IF NOT (v_rows[1] ? TG_ARGV[1]) OR (pg_catalog.left(TG_ARGV[0], 1) = '='
        AND NOT (v_rows[1] ? pg_catalog.substr(TG_ARGV[0], 2))) THEN
        RAISE EXCEPTION USING ERRCODE = 'XF002', CONSTRAINT = 'audited_change_config',
            SCHEMA = TG_TABLE_SCHEMA, TABLE = TG_TABLE_NAME,
            MESSAGE = pg_catalog.format(
                'the audit trigger of forge.%s names a column the table lacks', TG_TABLE_NAME);
    END IF;
    IF TG_OP = 'UPDATE' THEN
        v_old := pg_catalog.to_jsonb(OLD) - v_skip;
        v_new := pg_catalog.to_jsonb(NEW) - v_skip;
        IF v_old = v_new THEN
            RETURN NULL;
        END IF;
        IF (v_old - v_txn) = (v_new - v_txn) THEN
            -- Only txn-audited columns changed (a subtree path rewrite): one
            -- succeeded audit row of the same resource type covers every row.
            -- The resource columns are never txn-audited (the ledger refuses
            -- it), so NEW names the type.
            IF pg_catalog.left(TG_ARGV[0], 1) = '=' THEN
                v_type := v_rows[1] ->> pg_catalog.substr(TG_ARGV[0], 2);
            ELSE
                v_type := TG_ARGV[0];
            END IF;
            IF EXISTS (
                SELECT 1 FROM ONLY forge.audit_log AS a
                WHERE a.txid = pg_catalog.pg_current_xact_id() AND a.outcome = 'succeeded'
                  AND a.resource_type = v_type
            ) THEN
                RETURN NULL;
            END IF;
            RAISE EXCEPTION USING ERRCODE = 'XF002', CONSTRAINT = 'audited_change',
                SCHEMA = TG_TABLE_SCHEMA, TABLE = TG_TABLE_NAME,
                MESSAGE = pg_catalog.format(
                    'a change of forge.%s has no audit row in its transaction', TG_TABLE_NAME);
        END IF;
    END IF;
    FOREACH v_r IN ARRAY v_rows LOOP
        IF pg_catalog.left(TG_ARGV[0], 1) = '=' THEN
            v_type := v_r ->> pg_catalog.substr(TG_ARGV[0], 2);
        ELSE
            v_type := TG_ARGV[0];
        END IF;
        v_idtext := v_r ->> TG_ARGV[1];
        -- Shape first, range second: the cast never sees a non-numeric value,
        -- so no row value can reach an error message.
        IF v_idtext !~ '^-?[0-9]{1,19}\Z' THEN
            RAISE EXCEPTION USING ERRCODE = 'XF002', CONSTRAINT = 'audited_change_config',
                SCHEMA = TG_TABLE_SCHEMA, TABLE = TG_TABLE_NAME,
                MESSAGE = pg_catalog.format(
                    'the audit id column of forge.%s does not hold an integer', TG_TABLE_NAME);
        END IF;
        IF v_idtext::numeric NOT BETWEEN -9223372036854775808 AND 9223372036854775807 THEN
            RAISE EXCEPTION USING ERRCODE = 'XF002', CONSTRAINT = 'audited_change_config',
                SCHEMA = TG_TABLE_SCHEMA, TABLE = TG_TABLE_NAME,
                MESSAGE = pg_catalog.format(
                    'the audit id column of forge.%s does not hold an integer', TG_TABLE_NAME);
        END IF;
        v_id := v_idtext::bigint;
        IF NOT EXISTS (
            SELECT 1 FROM ONLY forge.audit_log AS a
            WHERE a.txid = pg_catalog.pg_current_xact_id()
              AND a.resource_type = v_type AND a.resource_id = v_id
              AND a.outcome = 'succeeded'
        ) THEN
            RAISE EXCEPTION USING ERRCODE = 'XF002', CONSTRAINT = 'audited_change',
                SCHEMA = TG_TABLE_SCHEMA, TABLE = TG_TABLE_NAME,
                MESSAGE = pg_catalog.format(
                    'a change of forge.%s has no audit row for its resource in its transaction',
                    TG_TABLE_NAME);
        END IF;
    END LOOP;
    RETURN NULL;
END
$$;
REVOKE ALL ON FUNCTION forge.audited_change_present() FROM PUBLIC;

INSERT INTO forge_audit.guarded_objects (object_type, object_identity) VALUES
    ('function', 'forge.audited_change_present()')
ON CONFLICT DO NOTHING;

INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, details)
VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate',
    '{"version": 20261028100000, "guarded_changes": ["forge.audited_change_present"]}');
