# forge-db migrations

Forward-only, append-only (CLAUDE.md §3-6, ADR-0014). File name:
`YYYYMMDDHHMMSS_<description>.sql` (UTC timestamp: no collisions between
parallel branches). An applied migration is never edited — the
`migration-immutability` hook blocks it; fix forward with a new file.
Each file runs in one transaction. Applied with `forge db migrate`, as
forge_owner (ADR-0018). The root gate `migrations` checks every file:

- every object is qualified `forge.`; the only other write is
  `INSERT INTO forge_audit.guarded_objects (object_type, object_identity)`
  for the guarded objects the file creates;
- every new table is in `../tables.toml` (the ledger: `guarded`, `kind`,
  `ro_columns`) — tests `every_table_is_in_the_ledger` and
  `guarded_objects_match_the_ledger`;
- SECURITY DEFINER functions pin `search_path = pg_catalog, forge, pg_temp`
  and revoke PUBLIC;
- the last statement is the file's own `forge.audit_log` row with
  `action = 'db.migrate'` and `details.guarded_changes` naming every
  guarded object it touches — a trigger, rule, policy or constraint by its
  own name `forge.<table>.<name>`, a function without arguments — so
  `forge_audit.ddl_audit_status()` marks its DDL expected.

A new table also needs its privileges in `src/bootstrap/schema.rs` (doctor
compares them exactly); a new or replaced guard function is compared with
its last `CREATE [OR REPLACE] FUNCTION` in these files.

**A state table** (`kind = "audited"`, step D1): declare `audit_resource`,
`audit_id_column`, `unaudited_columns` and `txn_audited_columns` in the
ledger, then attach the audit trigger in the same file:

```sql
CREATE CONSTRAINT TRIGGER audited_change_present
    AFTER INSERT OR DELETE OR UPDATE OF <every column but unaudited_columns> ON forge.<table>
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION forge.audited_change_present('<audit_resource>', '<audit_id_column>',
        '<unaudited, comma separated>', '<txn-audited, comma separated>');
ALTER TABLE forge.<table> ENABLE ALWAYS TRIGGER audited_change_present;
```

A later migration that adds a column must re-create the trigger with the
new column in `UPDATE OF` (or list the column as unaudited in the ledger):
doctor reports `guard=audited_trigger` otherwise. Name the trigger in
`guarded_changes` as `forge.<table>.audited_change_present`.
