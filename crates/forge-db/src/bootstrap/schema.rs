//! Guards of the migrated schema (ADR-0016 §1, ADR-0018 §3): the append-only
//! triggers of events and audit_log and the exact privileges of the tables
//! the first migration creates. Before that migration the tables do not
//! exist; the schema check reports the pending migration instead.

use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::acl::{compare, table_grants_sql};
use super::audited::{check_audited_triggers, Ledger};
use super::guard_missing;
use crate::read_query_error;

/// The guard functions of the migrated schema.
pub(crate) const GUARD_FUNCTIONS: &[&str] = &[
    "forge.events_guard()",
    "forge.audit_log_guard()",
    "forge.event_feed_state_guard()",
    "forge.events_audit_present()",
    // ── C8 ──
    "forge.audit_seal_guard()",
    // ── end C8 ──
    // ── C5: derived-event checks (migration 20261027140000) ──
    "forge.events_lineage()",
    "forge.events_derived_progress()",
    // ── end C5 ──
    // ── D1: the audit trigger of state tables (migration 20261028100000) ──
    "forge.audited_change_present()",
    // ── D4: repositories move only provisioning -> ready (20261031100000) ──
    "forge.repositories_state_guard()",
    // ── D5 MR2: deletes for the purge only (20261102100100) ──
    "forge.purge_delete_guard()",
    // ── M2 MR3: a session's revocation is final (20261103100100) ──
    "forge.web_sessions_revocation_guard()",
    // ── M2 MR6: sessions are deleted only 30 days after they ended (20261105100000) ──
    "forge.web_sessions_purge_guard()",
    // ── M3 R1a: merged is final (20261106100000) ──
    "forge.merge_requests_state_guard()",
    // ── M3 R1b: a version's identity never changes (20261107100000) ──
    "forge.merge_request_versions_guard()",
    // ── M3 R3: discussions keep their position, notes are final (20261109100000) ──
    "forge.merge_request_discussions_guard()",
    "forge.merge_request_notes_guard()",
    // ── M3 R4: a merge attempt only moves forward (20261110100000) ──
    "forge.merge_request_merges_guard()",
    // ── M3 R5: an approval ends once (20261111100000) ──
    "forge.merge_request_approvals_guard()",
    // ── M3 R5 re-review: unlisted ref updates never change (20261111100200) ──
    "forge.push_unlisted_refs_guard()",
    // ── M3 R5 round 4: pushed commits never change (20261111100300) ──
    "forge.push_commits_guard()",
    // ── M3 S0 review: 20261113100100, 20261113100200 ──
    "forge.users_kind_immutable()",
    "forge.service_account_profiles_kind_guard()",
    "forge.user_identities_human_guard()",
    // ── M3 I1a: an integration's service account (20261114100000) ──
    "forge.integrations_service_account_guard()",
    // ── M3 C1: check runs and namespace grants (20261115100000) ──
    "forge.check_runs_guard()",
    "forge.check_namespace_grants_kind_guard()",
    // ── M3 I1b: a work item link ends once; the purge deletes it (20261116100000) ──
    "forge.merge_request_work_items_guard()",
    // ── M3 I1b review: a person's link is audited, a detected one the
    // subscriber's (20261116100100) ──
    "forge.merge_request_work_items_audited()",
    // ── M3 S2: employee numbers kept; assertions move forward (20261125100000) ──
    "forge.users_employee_number_kept_when_disabled()",
    "forge.act_as_assertions_guard()",
    // ── M3 X3 review: a resolving binding is alone on its project (20261129100000) ──
    "forge.integration_bindings_resolving_alone()",
];

/// Guard triggers of state tables (table, trigger, `pg_get_triggerdef`):
/// each must exist with this exact definition, enabled ALWAYS. Unlike
/// `GUARDED_TRIGGERS`, the table may have other triggers.
const STATE_GUARD_TRIGGERS: &[(&str, &str, &str)] = &[
    // ── M2 MR3: a session's revocation is final (20261103100100) ──
    (
        "web_sessions",
        "web_sessions_revocation_final",
        "CREATE TRIGGER web_sessions_revocation_final BEFORE UPDATE ON forge.web_sessions \
         FOR EACH ROW EXECUTE FUNCTION forge.web_sessions_revocation_guard()",
    ),
    // ── M2 MR6: the purge's DELETE only 30 days after a session ended (20261105100000) ──
    (
        "web_sessions",
        "web_sessions_purge_only",
        "CREATE TRIGGER web_sessions_purge_only BEFORE DELETE ON forge.web_sessions \
         FOR EACH ROW EXECUTE FUNCTION forge.web_sessions_purge_guard()",
    ),
    (
        "repositories",
        "repositories_state_forward",
        "CREATE TRIGGER repositories_state_forward BEFORE UPDATE ON forge.repositories \
         FOR EACH ROW EXECUTE FUNCTION forge.repositories_state_guard()",
    ),
    // ── M3 R1a: merged is final (20261106100000) ──
    (
        "merge_requests",
        "merge_requests_state_forward",
        "CREATE TRIGGER merge_requests_state_forward BEFORE UPDATE ON forge.merge_requests \
         FOR EACH ROW EXECUTE FUNCTION forge.merge_requests_state_guard()",
    ),
    // ── M3 R1b: a version's identity never changes (20261107100000) ──
    (
        "merge_request_versions",
        "merge_request_versions_set_once",
        "CREATE TRIGGER merge_request_versions_set_once BEFORE UPDATE ON \
         forge.merge_request_versions FOR EACH ROW EXECUTE FUNCTION \
         forge.merge_request_versions_guard()",
    ),
    // ── M3 R3: discussions keep their position, notes are final (20261109100000) ──
    (
        "merge_request_discussions",
        "merge_request_discussions_identity",
        "CREATE TRIGGER merge_request_discussions_identity BEFORE UPDATE ON \
         forge.merge_request_discussions FOR EACH ROW EXECUTE FUNCTION \
         forge.merge_request_discussions_guard()",
    ),
    (
        "merge_request_notes",
        "merge_request_notes_immutable",
        "CREATE TRIGGER merge_request_notes_immutable BEFORE UPDATE ON \
         forge.merge_request_notes FOR EACH ROW EXECUTE FUNCTION \
         forge.merge_request_notes_guard()",
    ),
    // ── M3 R4: a merge attempt starts queued and only moves forward
    // (20261110100000, INSERT too since 20261110100100) ──
    (
        "merge_request_merges",
        "merge_request_merges_forward",
        "CREATE TRIGGER merge_request_merges_forward BEFORE INSERT OR UPDATE ON \
         forge.merge_request_merges FOR EACH ROW EXECUTE FUNCTION \
         forge.merge_request_merges_guard()",
    ),
    // ── M3 R5: an approval's identity never changes; it ends once (20261111100000) ──
    (
        "merge_request_approvals",
        "merge_request_approvals_end_once",
        "CREATE TRIGGER merge_request_approvals_end_once BEFORE UPDATE ON \
         forge.merge_request_approvals FOR EACH ROW EXECUTE FUNCTION \
         forge.merge_request_approvals_guard()",
    ),
    // ── M3 R5 re-review: unlisted ref updates never change (20261111100200) ──
    (
        "push_unlisted_refs",
        "push_unlisted_refs_immutable",
        "CREATE TRIGGER push_unlisted_refs_immutable BEFORE UPDATE ON \
         forge.push_unlisted_refs FOR EACH ROW EXECUTE FUNCTION \
         forge.push_unlisted_refs_guard()",
    ),
    // ── M3 R5 round 4: pushed commits never change (20261111100300) ──
    (
        "push_commits",
        "push_commits_immutable",
        "CREATE TRIGGER push_commits_immutable BEFORE UPDATE ON \
         forge.push_commits FOR EACH ROW EXECUTE FUNCTION \
         forge.push_commits_guard()",
    ),
    // ── M3 S0: a user's kind never changes; the kinds of a profile and an
    // identity (20261113100000, guarded since 20261113100100) ──
    (
        "users",
        "users_kind_immutable",
        "CREATE TRIGGER users_kind_immutable BEFORE UPDATE OF kind ON forge.users \
         FOR EACH ROW EXECUTE FUNCTION forge.users_kind_immutable()",
    ),
    (
        "service_account_profiles",
        "service_account_profiles_kind_guard",
        "CREATE TRIGGER service_account_profiles_kind_guard BEFORE INSERT OR UPDATE OF \
         user_id, owner_user_id ON forge.service_account_profiles FOR EACH ROW EXECUTE \
         FUNCTION forge.service_account_profiles_kind_guard()",
    ),
    (
        "user_identities",
        "user_identities_human_guard",
        "CREATE TRIGGER user_identities_human_guard BEFORE INSERT OR UPDATE OF user_id ON \
         forge.user_identities FOR EACH ROW EXECUTE FUNCTION forge.user_identities_human_guard()",
    ),
    // ── M3 S2: a disabled user keeps its employee number (20261125100000) ──
    (
        "users",
        "users_employee_number_kept_when_disabled",
        "CREATE TRIGGER users_employee_number_kept_when_disabled BEFORE UPDATE OF \
         employee_number, state ON forge.users FOR EACH ROW EXECUTE FUNCTION \
         forge.users_employee_number_kept_when_disabled()",
    ),
    // ── M3 S2: an assertion only moves forward (20261125100000) ──
    (
        "act_as_assertions",
        "act_as_assertions_forward",
        "CREATE TRIGGER act_as_assertions_forward BEFORE INSERT OR UPDATE ON \
         forge.act_as_assertions FOR EACH ROW EXECUTE FUNCTION \
         forge.act_as_assertions_guard()",
    ),
    // ── M3 X3 review: a resolving binding is alone on its project (20261129100000) ──
    (
        "integration_bindings",
        "integration_bindings_resolving_alone",
        "CREATE CONSTRAINT TRIGGER integration_bindings_resolving_alone AFTER INSERT OR UPDATE \
         OF project_id, resolves_work_items ON forge.integration_bindings DEFERRABLE INITIALLY \
         DEFERRED FOR EACH ROW EXECUTE FUNCTION forge.integration_bindings_resolving_alone()",
    ),
    // ── M3 I1a: an integration acts as one service account (20261114100000) ──
    (
        "integrations",
        "integrations_service_account_guard",
        "CREATE TRIGGER integrations_service_account_guard BEFORE INSERT OR UPDATE OF \
         service_account_id ON forge.integrations FOR EACH ROW EXECUTE FUNCTION \
         forge.integrations_service_account_guard()",
    ),
    // ── M3 C1: a check run's identity; grants to service accounts only (20261115100000) ──
    (
        "check_runs",
        "check_runs_identity",
        "CREATE TRIGGER check_runs_identity BEFORE UPDATE ON forge.check_runs \
         FOR EACH ROW EXECUTE FUNCTION forge.check_runs_guard()",
    ),
    (
        "check_namespace_grants",
        "check_namespace_grants_kind_guard",
        "CREATE TRIGGER check_namespace_grants_kind_guard BEFORE INSERT OR UPDATE OF user_id \
         ON forge.check_namespace_grants FOR EACH ROW EXECUTE FUNCTION \
         forge.check_namespace_grants_kind_guard()",
    ),
    // ── M3 I1b: a work item link ends once; the purge deletes it (20261116100000) ──
    (
        "merge_request_work_items",
        "merge_request_work_items_end_once",
        "CREATE TRIGGER merge_request_work_items_end_once BEFORE DELETE OR UPDATE ON \
         forge.merge_request_work_items FOR EACH ROW EXECUTE FUNCTION \
         forge.merge_request_work_items_guard()",
    ),
    // ── M3 I1b review: deferred, a person's link needs its audit row
    // (20261116100100) ──
    (
        "merge_request_work_items",
        "merge_request_work_items_audited",
        "CREATE CONSTRAINT TRIGGER merge_request_work_items_audited AFTER INSERT OR UPDATE OF \
         unlinked_at ON forge.merge_request_work_items DEFERRABLE INITIALLY DEFERRED FOR EACH \
         ROW EXECUTE FUNCTION forge.merge_request_work_items_audited()",
    ),
    // ── D5 MR2: deletes for the purge only (20261102100100) ──
    (
        "repositories",
        "repositories_purge_only",
        "CREATE TRIGGER repositories_purge_only BEFORE DELETE ON forge.repositories \
         FOR EACH ROW EXECUTE FUNCTION forge.purge_delete_guard()",
    ),
    (
        "projects",
        "projects_purge_only",
        "CREATE TRIGGER projects_purge_only BEFORE DELETE ON forge.projects \
         FOR EACH ROW EXECUTE FUNCTION forge.purge_delete_guard()",
    ),
    (
        "namespaces",
        "namespaces_purge_only",
        "CREATE TRIGGER namespaces_purge_only BEFORE DELETE ON forge.namespaces \
         FOR EACH ROW EXECUTE FUNCTION forge.purge_delete_guard()",
    ),
];

/// Every trigger of each guarded table, as `pg_get_triggerdef` prints it
/// (with `search_path = pg_catalog`): no other trigger may exist.
const GUARDED_TRIGGERS: &[(&str, &[(&str, &str)])] = &[
    (
        "events",
        &[
            (
                "events_append_only",
                "CREATE TRIGGER events_append_only BEFORE INSERT OR DELETE OR UPDATE ON \
                 forge.events FOR EACH ROW EXECUTE FUNCTION forge.events_guard()",
            ),
            (
                "events_no_truncate",
                "CREATE TRIGGER events_no_truncate BEFORE TRUNCATE ON forge.events FOR EACH \
                 STATEMENT EXECUTE FUNCTION forge.events_guard()",
            ),
            // ── C5: plain events only again, plus the derived-event checks ──
            (
                "events_audit_present",
                "CREATE CONSTRAINT TRIGGER events_audit_present AFTER INSERT ON forge.events \
                 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW \
                 WHEN ((new.caused_by_seq IS NULL)) \
                 EXECUTE FUNCTION forge.events_audit_present()",
            ),
            (
                "events_lineage",
                "CREATE TRIGGER events_lineage BEFORE INSERT ON forge.events FOR EACH ROW \
                 WHEN ((new.caused_by_seq IS NOT NULL)) \
                 EXECUTE FUNCTION forge.events_lineage()",
            ),
            (
                "events_derived_progress",
                "CREATE CONSTRAINT TRIGGER events_derived_progress AFTER INSERT ON \
                 forge.events DEFERRABLE INITIALLY DEFERRED FOR EACH ROW \
                 WHEN ((new.caused_by_seq IS NOT NULL)) \
                 EXECUTE FUNCTION forge.events_derived_progress()",
            ),
            // ── end C5 ──
        ],
    ),
    (
        "audit_log",
        &[
            (
                "audit_log_append_only",
                "CREATE TRIGGER audit_log_append_only BEFORE INSERT OR DELETE OR UPDATE ON \
                 forge.audit_log FOR EACH ROW EXECUTE FUNCTION forge.audit_log_guard()",
            ),
            (
                "audit_log_no_truncate",
                "CREATE TRIGGER audit_log_no_truncate BEFORE TRUNCATE ON forge.audit_log FOR \
                 EACH STATEMENT EXECUTE FUNCTION forge.audit_log_guard()",
            ),
        ],
    ),
    (
        "event_feed_state",
        &[
            (
                "event_feed_state_append_only",
                "CREATE TRIGGER event_feed_state_append_only BEFORE DELETE OR UPDATE ON \
                 forge.event_feed_state FOR EACH ROW EXECUTE FUNCTION \
                 forge.event_feed_state_guard()",
            ),
            (
                "event_feed_state_no_truncate",
                "CREATE TRIGGER event_feed_state_no_truncate BEFORE TRUNCATE ON \
                 forge.event_feed_state FOR EACH STATEMENT EXECUTE FUNCTION \
                 forge.event_feed_state_guard()",
            ),
        ],
    ),
    // ── C8 ──
    (
        "audit_seal",
        &[
            (
                "audit_seal_append_only",
                "CREATE TRIGGER audit_seal_append_only BEFORE INSERT OR DELETE OR UPDATE ON \
                 forge.audit_seal FOR EACH ROW EXECUTE FUNCTION forge.audit_seal_guard()",
            ),
            (
                "audit_seal_no_truncate",
                "CREATE TRIGGER audit_seal_no_truncate BEFORE TRUNCATE ON forge.audit_seal FOR \
                 EACH STATEMENT EXECUTE FUNCTION forge.audit_seal_guard()",
            ),
        ],
    ),
    // ── end C8 ──
];

const EVENT_INSERT: &[&str] = &[
    "event_id",
    "event_type",
    "schema_version",
    "aggregate_type",
    "aggregate_id",
    "project_id",
    "actor_kind",
    "actor_user_id",
    "actor_via_id",
    "request_id",
    "caused_by_seq",
    "root_seq",
    "causation_depth",
    "dedup_key",
    "payload",
];

const AUDIT_INSERT: &[&str] = &[
    "actor_kind",
    "actor_user_id",
    "actor_via_id",
    "credential_id",
    "act_as_claimed",
    "act_as_key_id",
    "via_assertion_id",
    "effective_scope",
    "actor_label",
    "action",
    "outcome",
    "resource_type",
    "resource_id",
    "project_id",
    "source_ip",
    "user_agent",
    "request_id",
    "error_code",
    "occurrences",
    "details",
];

fn columns(role: &str, privilege: &str, cols: &[&str]) -> Vec<String> {
    cols.iter()
        .map(|c| format!("{role}:{privilege}({c})"))
        .collect()
}

fn plain(grants: &[&str]) -> Vec<String> {
    grants.iter().map(|g| (*g).to_owned()).collect()
}

/// forge_ro's SELECT grants on one table, from the ledger's `ro_columns`
/// (tables.toml): `"*"` is a table-wide SELECT, a list is one column grant
/// each. The ledger is the single place a readable column is declared; a
/// table without an entry is a build defect, never "no grants".
pub(crate) fn ro_grants(ledger: &toml::Table, table: &str) -> Result<Vec<String>, ForgeError> {
    let columns = ledger
        .get("tables")
        .and_then(toml::Value::as_table)
        .and_then(|tables| tables.get(table))
        .and_then(toml::Value::as_table)
        .and_then(|entry| entry.get("ro_columns"))
        .and_then(toml::Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .map(toml::Value::as_str)
                .collect::<Option<Vec<_>>>()
        })
        .filter(|cols| !cols.is_empty())
        .ok_or_else(|| ledger_invalid("ro_columns_missing", table))?;
    if columns.contains(&"*") {
        return if columns.len() == 1 {
            Ok(plain(&["forge_ro:SELECT"]))
        } else {
            Err(ledger_invalid("ro_columns_star_mixed", table))
        };
    }
    Ok(columns
        .iter()
        .map(|c| format!("forge_ro:SELECT({c})"))
        .collect())
}

fn ledger_invalid(reason: &'static str, table: &str) -> ForgeError {
    // The ledger is built into the binary: a failure here is a build defect.
    ForgeError::new(ErrorCode::ConfigInvalid)
        .with_context("config_key", "tables.toml")
        .with_context("reason", reason)
        .with_context("table", table)
}

/// The grants of the first migration, per table.
fn expectations() -> Result<Vec<(&'static str, Vec<String>)>, ForgeError> {
    let ledger: toml::Table = include_str!("../../tables.toml")
        .parse()
        .map_err(|_| ledger_invalid("not_toml", "-"))?;
    let ro = |table: &str| ro_grants(&ledger, table);
    let dml = |role: &str| {
        ["SELECT", "INSERT", "UPDATE", "DELETE"]
            .iter()
            .map(|p| format!("{role}:{p}"))
            .collect::<Vec<_>>()
    };
    let operational = || [dml("forge_app"), plain(&["forge_ro:SELECT"])].concat();
    Ok(vec![
        (
            "forge.events",
            [
                plain(&[
                    "forge_app:SELECT",
                    "forge_seq:SELECT",
                    "forge_purge:SELECT",
                    "forge_purge:DELETE",
                    "forge_ro:SELECT",
                ]),
                columns("forge_app", "INSERT", EVENT_INSERT),
                columns("forge_seq", "UPDATE", &["seq", "sequenced_at"]),
                columns("forge_purge", "UPDATE", &["payload"]),
            ]
            .concat(),
        ),
        (
            "forge.audit_log",
            [
                plain(&[
                    "forge_app:SELECT",
                    "forge_seq:SELECT",
                    "forge_purge:SELECT",
                    "forge_purge:DELETE",
                ]),
                columns("forge_app", "INSERT", AUDIT_INSERT),
                columns("forge_seq", "INSERT", AUDIT_INSERT),
                columns("forge_purge", "INSERT", AUDIT_INSERT),
                columns("forge_seq", "UPDATE", &["seq", "sequenced_at"]),
                columns(
                    "forge_purge",
                    "UPDATE",
                    &["actor_label", "source_ip", "user_agent"],
                ),
                ro("audit_log")?,
                columns("forge_maint", "SELECT", &["txid", "action", "details"]),
            ]
            .concat(),
        ),
        (
            "forge.event_feed_state",
            [
                plain(&[
                    "forge_app:SELECT",
                    "forge_seq:SELECT",
                    "forge_seq:INSERT",
                    "forge_seq:UPDATE",
                    "forge_purge:SELECT",
                    "forge_ro:SELECT",
                ]),
                columns("forge_purge", "UPDATE", &["retained_from"]),
            ]
            .concat(),
        ),
        (
            "forge.event_subscribers",
            [
                plain(&["forge_app:SELECT", "forge_purge:SELECT", "forge_ro:SELECT"]),
                columns(
                    "forge_app",
                    "INSERT",
                    &["name", "ordering", "last_seq", "last_txid", "updated_at"],
                ),
                columns(
                    "forge_app",
                    "UPDATE",
                    &[
                        "last_seq",
                        "last_txid",
                        "updated_at",
                        "failures",
                        "failing_since",
                        "last_error_code",
                    ],
                ),
            ]
            .concat(),
        ),
        (
            "forge.subscriber_dead_letters",
            plain(&[
                "forge_app:SELECT",
                "forge_app:INSERT",
                "forge_purge:SELECT",
                "forge_purge:DELETE",
                "forge_ro:SELECT",
            ]),
        ),
        ("forge.jobs", operational()),
        // ── C8 ──
        (
            "forge.audit_seal",
            [
                plain(&[
                    "forge_app:SELECT",
                    "forge_seq:SELECT",
                    "forge_purge:SELECT",
                    "forge_ro:SELECT",
                ]),
                columns("forge_app", "INSERT", SEAL_INSERT),
                columns("forge_seq", "INSERT", SEAL_INSERT),
                columns("forge_app", "UPDATE", &["shipped_at"]),
            ]
            .concat(),
        ),
        // ── end C8 ──
        // ── D1: no DELETE on users / namespaces yet, no UPDATE on tombstones,
        // TRUNCATE for no one (it bypasses the audit trigger) ──
        (
            "forge.users",
            [
                plain(&["forge_app:SELECT", "forge_app:INSERT", "forge_app:UPDATE"]),
                ro("users")?,
            ]
            .concat(),
        ),
        // D5 MR2: DELETE for the purge (migration 20261102100000).
        (
            "forge.namespaces",
            plain(&[
                "forge_app:SELECT",
                "forge_app:INSERT",
                "forge_app:UPDATE",
                "forge_app:DELETE",
                "forge_ro:SELECT",
            ]),
        ),
        (
            "forge.namespace_tombstones",
            plain(&[
                "forge_app:SELECT",
                "forge_app:INSERT",
                "forge_app:DELETE",
                "forge_ro:SELECT",
            ]),
        ),
        // ── end D1 ──
        // ── D2: no DELETE on groups (Q11), TRUNCATE for no one ──
        (
            "forge.groups",
            plain(&[
                "forge_app:SELECT",
                "forge_app:INSERT",
                "forge_app:UPDATE",
                "forge_ro:SELECT",
            ]),
        ),
        (
            "forge.group_members",
            plain(&[
                "forge_app:SELECT",
                "forge_app:INSERT",
                "forge_app:UPDATE",
                "forge_app:DELETE",
                "forge_ro:SELECT",
            ]),
        ),
        // ── end D2 ──
        // ── D3: tokens: no DELETE, UPDATE on three columns only, the read
        // pool without secret_sha256 (ADR-0018) ──
        (
            "forge.tokens",
            [
                plain(&["forge_app:SELECT", "forge_app:INSERT"]),
                columns("forge_app", "UPDATE", TOKENS_APP_UPDATE),
                ro("tokens")?,
            ]
            .concat(),
        ),
        // ── end D3 ──
        // ── D4: repository kind fixed (UPDATE of state and ready_at only),
        // the read pool without provision_token, TRUNCATE for no one; D5
        // MR2: DELETE on projects / repositories for the purge ──
        (
            "forge.projects",
            plain(&[
                "forge_app:SELECT",
                "forge_app:INSERT",
                "forge_app:UPDATE",
                "forge_app:DELETE",
                "forge_ro:SELECT",
            ]),
        ),
        (
            "forge.repositories",
            [
                plain(&["forge_app:SELECT", "forge_app:INSERT", "forge_app:DELETE"]),
                columns("forge_app", "UPDATE", &["state", "ready_at"]),
                ro("repositories")?,
            ]
            .concat(),
        ),
        ("forge.project_members", operational()),
        ("forge.protected_refs", operational()),
        // ── end D4 ──
        // ── M2 MR3: web sessions and IdP identities: no DELETE, UPDATE of
        // the listed columns only, the read pool without hashes, ciphertexts
        // or the IdP's subject (ADR-0021 §4–§5) ──
        (
            "forge.web_sessions",
            [
                // M2 MR6: DELETE for the purge (20261105100000, guarded).
                plain(&["forge_app:SELECT", "forge_app:INSERT", "forge_app:DELETE"]),
                columns("forge_app", "UPDATE", WEB_SESSIONS_APP_UPDATE),
                ro("web_sessions")?,
            ]
            .concat(),
        ),
        (
            "forge.user_identities",
            [
                plain(&["forge_app:SELECT", "forge_app:INSERT"]),
                columns("forge_app", "UPDATE", &["last_login_at"]),
                ro("user_identities")?,
            ]
            .concat(),
        ),
        // ── end M2 MR3 ──
        // ── M3 R1a: merge requests, DELETE for the purge (20261106100000) ──
        ("forge.merge_requests", operational()),
        ("forge.merge_request_counters", operational()),
        // ── end M3 R1a ──
        // ── M3 R1b: versions, DELETE for the purge (20261107100000) ──
        ("forge.merge_request_versions", operational()),
        // ── M3 R2: mergeability, DELETE for the purge (20261108100000) ──
        ("forge.merge_request_mergeability", operational()),
        // ── M3 X6: conflict predictions, DELETE for the purge (20261123100000) ──
        ("forge.merge_request_conflict_scans", operational()),
        ("forge.merge_request_conflict_pairs", operational()),
        // ── M3 R3: discussions and notes, DELETE for the purge (20261109100000) ──
        ("forge.merge_request_discussions", operational()),
        ("forge.merge_request_notes", operational()),
        // ── M3 R4: merge attempts, DELETE for the purge (20261110100000) ──
        ("forge.merge_request_merges", operational()),
        // ── M3 R5: approvals and rules, removed by the purge (20261111100000) ──
        ("forge.merge_request_approvals", operational()),
        ("forge.approval_rules", operational()),
        ("forge.approval_rule_approvers", operational()),
        ("forge.push_unlisted_refs", operational()),
        ("forge.push_commits", operational()),
        // ── M3 R6: merge conditions, removed by the purge (20261112100000) ──
        ("forge.merge_settings", operational()),
        ("forge.protected_ref_merge_rules", operational()),
        // ── M3 S0: service account profiles, no DELETE (20261113100000) ──
        (
            "forge.service_account_profiles",
            plain(&[
                "forge_app:SELECT",
                "forge_app:INSERT",
                "forge_app:UPDATE",
                "forge_ro:SELECT",
            ]),
        ),
        // ── M3 I1a: integrations (no DELETE) and their bindings (deleted
        // by unbinding and the purge; UPDATE of granted_membership only,
        // 20261114100100 / 20261114100200: a removal or role change of the
        // granted membership clears it) (20261114100000) ──
        (
            "forge.integrations",
            plain(&[
                "forge_app:SELECT",
                "forge_app:INSERT",
                "forge_app:UPDATE",
                "forge_ro:SELECT",
            ]),
        ),
        (
            "forge.integration_bindings",
            [
                plain(&["forge_app:SELECT", "forge_app:DELETE", "forge_ro:SELECT"]),
                // M3 I1c review (20261118100100): INSERT by column, never
                // `generation` (no OVERRIDING SYSTEM VALUE).
                columns(
                    "forge_app",
                    "INSERT",
                    &[
                        "integration_id",
                        "project_id",
                        "external_project_id",
                        "granted_membership",
                        "created_by",
                        // M3 X3 (20261129100000).
                        "resolves_work_items",
                    ],
                ),
                // Column-level since 20261114100200 (re-review);
                // resolves_work_items since M3 X3 (20261129100000).
                columns(
                    "forge_app",
                    "UPDATE",
                    &["granted_membership", "resolves_work_items"],
                ),
            ]
            .concat(),
        ),
        // ── M3 C1: check runs, annotations (removed by the purge) and the
        // namespace grants (a revoke deletes) (20261115100000) ──
        ("forge.check_runs", operational()),
        ("forge.check_annotations", operational()),
        ("forge.check_namespace_grants", operational()),
        // ── M3 O3: the maintenance ledger, DELETE for the purge (20261117100000) ──
        ("forge.repository_maintenance", operational()),
        // ── M3 I1b: work items (never updated or deleted), their links
        // (UPDATE of the unlink columns only, DELETE for the purge) and the
        // events' work items (20261116100000) ──
        (
            "forge.work_items",
            plain(&["forge_app:SELECT", "forge_app:INSERT", "forge_ro:SELECT"]),
        ),
        (
            "forge.merge_request_work_items",
            [
                plain(&["forge_app:SELECT", "forge_app:DELETE", "forge_ro:SELECT"]),
                // Column-level INSERT since 20261116100100 (not linked_at).
                columns(
                    "forge_app",
                    "INSERT",
                    &[
                        "merge_request_id",
                        "project_id",
                        "work_item_id",
                        "source",
                        "linked_by",
                        // 20261116100200: who linked by hand.
                        "linked_by_kind",
                        "linked_seq",
                    ],
                ),
                columns(
                    "forge_app",
                    "UPDATE",
                    &[
                        "unlinked_at",
                        "unlink_source",
                        "unlinked_by",
                        "unlinked_by_kind",
                    ],
                ),
            ]
            .concat(),
        ),
        (
            "forge.event_work_items",
            plain(&[
                "forge_app:SELECT",
                "forge_app:INSERT",
                "forge_ro:SELECT",
                // M3 I1c: deleted with their events by forge.events_retention.
                "forge_purge:SELECT",
                "forge_purge:DELETE",
            ]),
        ),
        // ── M3 I1c: the feed's reads (derived, written by the feed) and the
        // retention policy (read only; 20261118100000) ──
        (
            "forge.integration_feed_reads",
            plain(&[
                "forge_app:SELECT",
                "forge_app:INSERT",
                "forge_app:UPDATE",
                "forge_ro:SELECT",
            ]),
        ),
        (
            "forge.retention_policy",
            plain(&["forge_app:SELECT", "forge_ro:SELECT", "forge_purge:SELECT"]),
        ),
        // ── M3 S2: the assertions — no DELETE, INSERT and UPDATE by column,
        // the read pool without mac / state_hash (20261125100000) ──
        (
            "forge.act_as_assertions",
            [
                plain(&["forge_app:SELECT"]),
                columns("forge_app", "INSERT", ACT_AS_APP_INSERT),
                columns("forge_app", "UPDATE", ACT_AS_APP_UPDATE),
                ro("act_as_assertions")?,
            ]
            .concat(),
        ),
        // ── M3 I2: webhooks (the read pool without the sealed secrets) and
        // their deliveries (20261120200000) ──
        (
            "forge.webhooks",
            [dml("forge_app"), ro("webhooks")?].concat(),
        ),
        ("forge.webhook_deliveries", operational()),
        // ── M3 N1: notifications (20261120100000) ──
        (
            "forge.notification_preferences",
            plain(&[
                "forge_app:SELECT",
                "forge_app:INSERT",
                "forge_app:UPDATE",
                "forge_ro:SELECT",
            ]),
        ),
        (
            "forge.notifications",
            plain(&[
                "forge_app:SELECT",
                "forge_app:INSERT",
                "forge_app:UPDATE",
                "forge_ro:SELECT",
            ]),
        ),
        // ── M3 U4: in-app notifications (20261201100000) ──
        (
            "forge.in_app_notifications",
            plain(&[
                "forge_app:SELECT",
                "forge_app:INSERT",
                "forge_app:UPDATE",
                "forge_app:DELETE",
                "forge_ro:SELECT",
            ]),
        ),
        // ── M3 O2: the push mirror (20261130100000) ──
        (
            "forge.push_mirrors",
            [dml("forge_app"), ro("push_mirrors")?].concat(),
        ),
        ("forge.push_mirror_status", operational()),
    ])
}

/// The columns forge_app may insert into forge.act_as_assertions.
const ACT_AS_APP_INSERT: &[&str] = &[
    "integration_id",
    "user_id",
    "intent",
    "project_id",
    "merge_request_id",
    "head_sha",
    "target_ref",
    "target_sha",
    // M3 X3: the binding intents (20261129100000).
    "external_project_id",
    "resolves_work_items",
    "state_hash",
    "expires_at",
    "issued_request_id",
];

/// The columns forge_app may update on forge.act_as_assertions.
const ACT_AS_APP_UPDATE: &[&str] = &[
    "state",
    "public_id",
    "mac",
    "key_id",
    "claimed_at",
    "finished_at",
    "reject_reason",
    "claimed_request_id",
    "finished_request_id",
];

/// The columns forge_app may update on forge.web_sessions.
const WEB_SESSIONS_APP_UPDATE: &[&str] = &[
    "last_seen_at",
    "idle_expires_at",
    "revoked_at",
    "revoked_reason",
    "refresh_token_enc",
    "id_token_enc",
    "refresh_checked_at",
    "access_role_seen_at",
    "refresh_without_id_token",
];

/// The columns forge_app may update on forge.tokens.
const TOKENS_APP_UPDATE: &[&str] = &["revoked_at", "revoked_reason", "last_used_at"];

// ── C8 ──
/// The columns forge_app and forge_seq may insert into forge.audit_seal
/// (not id, sealed_at, shipped_at).
const SEAL_INSERT: &[&str] = &[
    "kind",
    "hash_version",
    "seal_epoch",
    "key_id",
    "first_seq",
    "last_seq",
    "row_count",
    "events_high_seq",
    "rows_sha256",
    "prev_seal_sha256",
    "seal_sha256",
    "hmac",
];
// ── end C8 ──

/// The first schema migration (events, audit_log, …).
const FIRST_SCHEMA_VERSION: i64 = 20261027100000;

/// Tables of later migrations, checked the same way (each phase adds its
/// own line).
const LATER_TABLES: &[&str] = &[
    // ── C8 ──
    "audit_seal",
    // ── end C8 ──
    // ── D1: state tables (migration 20261028100100) ──
    "users",
    "namespaces",
    "namespace_tombstones",
    // ── end D1 ──
    // ── D2: groups (migration 20261029100000) ──
    "groups",
    "group_members",
    // ── end D2 ──
    // ── D3: tokens (migration 20261030100000) ──
    "tokens",
    // ── end D3 ──
    // ── D4: projects (migration 20261031100000) ──
    "projects",
    "repositories",
    "project_members",
    "protected_refs",
    // ── end D4 ──
    // ── M2 MR3: web sessions (migration 20261103100000) ──
    "web_sessions",
    "user_identities",
    // ── end M2 MR3 ──
    // ── M3 R1a: merge requests (migration 20261106100000) ──
    "merge_requests",
    "merge_request_counters",
    // ── end M3 R1a ──
    // ── M3 R1b: merge request versions (migration 20261107100000) ──
    "merge_request_versions",
    // ── M3 R2: mergeability (migration 20261108100000) ──
    "merge_request_mergeability",
    // ── M3 X6: conflict predictions (migration 20261123100000) ──
    "merge_request_conflict_scans",
    "merge_request_conflict_pairs",
    // ── M3 R3: discussions and notes (migration 20261109100000) ──
    "merge_request_discussions",
    "merge_request_notes",
    // ── M3 R4: merge attempts (migration 20261110100000) ──
    "merge_request_merges",
    // ── M3 R5: approvals and approval rules (migration 20261111100000) ──
    "merge_request_approvals",
    "approval_rules",
    "approval_rule_approvers",
    // ── M3 R5 re-review (migration 20261111100200) ──
    "push_unlisted_refs",
    // ── M3 R5 round 4 (migration 20261111100300) ──
    "push_commits",
    // ── M3 R6: merge conditions (migration 20261112100000) ──
    "merge_settings",
    "protected_ref_merge_rules",
    // ── M3 S0: service accounts (migration 20261113100000) ──
    "service_account_profiles",
    // ── M3 I1a: integrations (migration 20261114100000) ──
    "integrations",
    "integration_bindings",
    // ── M3 C1: the Checks API (migration 20261115100000) ──
    "check_runs",
    "check_annotations",
    "check_namespace_grants",
    // ── M3 I1c: the event feed (migration 20261118100000) ──
    "integration_feed_reads",
    "retention_policy",
    // ── M3 O3: the maintenance ledger (migration 20261117100000) ──
    "repository_maintenance",
    // ── M3 S2: acting for a person (migration 20261125100000) ──
    "act_as_assertions",
    // ── M3 I2: webhooks (migration 20261120200000) ──
    "webhooks",
    "webhook_deliveries",
    // ── M3 N1: notifications (migration 20261120100000) ──
    "notification_preferences",
    "notifications",
    // ── M3 U4: in-app notifications (migration 20261201100000) ──
    "in_app_notifications",
    // ── M3 O2: the push mirror (migration 20261130100000) ──
    "push_mirrors",
    "push_mirror_status",
];

/// Every table the first migration creates.
const SCHEMA_TABLES: &[&str] = &[
    "events",
    "audit_log",
    "event_feed_state",
    "event_subscribers",
    "subscriber_dead_letters",
    "jobs",
];

/// Whether the first schema migration is recorded as applied. Before it,
/// the schema check reports it pending; after it, a missing table is a
/// guard failure, never a skip.
pub(super) async fn schema_applied(conn: &mut PgConnection) -> Result<bool, ForgeError> {
    // Either the bookkeeping says so, or the tables are there (forge_owner
    // can delete the bookkeeping row with plain DML, which the DDL log does
    // not see).
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM forge._sqlx_migrations WHERE version = $1 AND success) \
         OR to_regclass('forge.audit_log') IS NOT NULL",
    )
    .bind(FIRST_SCHEMA_VERSION)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.schema_applied", e))
}

pub(super) async fn check_schema_objects(conn: &mut PgConnection) -> Result<(), ForgeError> {
    check_schema_objects_with(conn, &Ledger::embedded()?).await
}

/// `check_schema_objects` against a given ledger (tests inject one with
/// audited probe tables).
pub(crate) async fn check_schema_objects_with(
    conn: &mut PgConnection,
    ledger: &Ledger,
) -> Result<(), ForgeError> {
    if !schema_applied(conn).await? {
        return Ok(());
    }
    // Present, owned by forge_owner, no rules, no row security.
    let rows: Vec<(String, bool)> = sqlx::query_as(
        "SELECT c.relname::text, \
         pg_catalog.pg_get_userbyid(c.relowner) = 'forge_owner' \
         AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_rewrite rw WHERE rw.ev_class = c.oid) \
         AND NOT c.relrowsecurity AND NOT c.relforcerowsecurity \
         AND c.relkind = 'r' AND NOT c.relispartition \
         AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_inherits i \
                         WHERE i.inhparent = c.oid OR i.inhrelid = c.oid) \
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'forge' AND c.relname = ANY($1)",
    )
    .bind(
        [SCHEMA_TABLES, LATER_TABLES, &["_sqlx_migrations"]]
            .concat()
            .iter()
            .map(|t| (*t).to_owned())
            .collect::<Vec<_>>(),
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.schema_tables", e))?;
    for table in SCHEMA_TABLES
        .iter()
        .chain(LATER_TABLES)
        .chain(&["_sqlx_migrations"])
    {
        if !rows.iter().any(|(name, ok)| name == table && *ok) {
            return Err(
                guard_missing("object_owner").with_context("object", format!("forge.{table}"))
            );
        }
    }
    for function in GUARD_FUNCTIONS {
        check_guard_function(conn, function).await?;
    }
    // D1: the audit trigger of every state table, against the ledger.
    check_audited_triggers(conn, ledger).await?;
    // An audited table's owner, rules, row security and exact ACL are
    // checked like every table's: it must be listed here, or a TRUNCATE
    // grant (which bypasses its row trigger) would go unnoticed.
    let expected = expectations()?;
    for table in &ledger.audited {
        let object = format!("forge.{}", table.name);
        let listed = LATER_TABLES.contains(&table.name.as_str())
            && expected.iter().any(|(name, _)| *name == object);
        if !listed {
            return Err(guard_missing("privileges")
                .with_context("object", object)
                .with_context("detail", "audited_table_unlisted"));
        }
    }
    for (table, expected) in GUARDED_TRIGGERS {
        // The whole definition (timing, events, level, WHEN, columns,
        // deferral, function) as PostgreSQL prints it, and enabled ALWAYS.
        let rows: Vec<(String, String, String)> = sqlx::query_as(&format!(
            "SELECT t.tgname::text, t.tgenabled::text, pg_catalog.pg_get_triggerdef(t.oid) \
             FROM pg_catalog.pg_trigger t \
             WHERE t.tgrelid = 'forge.{table}'::regclass AND NOT t.tgisinternal"
        ))
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| read_query_error("bootstrap.schema_triggers", e))?;
        for (name, definition) in *expected {
            let ok = rows
                .iter()
                .any(|(n, enabled, def)| n == name && enabled == "A" && def == definition);
            if !ok {
                return Err(guard_missing("log_triggers").with_context("trigger", *name));
            }
        }
        if let Some((extra, ..)) = rows
            .iter()
            .find(|(n, ..)| !expected.iter().any(|(name, _)| name == n))
        {
            return Err(guard_missing("log_triggers").with_context("trigger", extra));
        }
    }
    // D4: guard triggers of state tables. Other triggers may exist there
    // (the audit trigger, checked against the ledger above).
    for (table, name, definition) in STATE_GUARD_TRIGGERS {
        let found: Option<(String, String)> = sqlx::query_as(&format!(
            "SELECT t.tgenabled::text, pg_catalog.pg_get_triggerdef(t.oid) \
             FROM pg_catalog.pg_trigger t \
             WHERE t.tgrelid = 'forge.{table}'::regclass AND NOT t.tgisinternal \
             AND t.tgname = $1"
        ))
        .bind(*name)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| read_query_error("bootstrap.schema_triggers", e))?;
        if !found.is_some_and(|(enabled, def)| enabled == "A" && def == *definition) {
            return Err(guard_missing("log_triggers").with_context("trigger", *name));
        }
    }
    for (table, grants) in expected {
        compare(conn, table, &table_grants_sql(table), &grants).await?;
    }
    Ok(())
}

/// A guard function as the migrations ship it: owned by forge_owner, not
/// SECURITY DEFINER, plpgsql, `search_path = pg_catalog, pg_temp`, and the
/// body of its last definition in the migrations built into this binary.
async fn check_guard_function(conn: &mut PgConnection, signature: &str) -> Result<(), ForgeError> {
    let name = signature
        .trim_start_matches("forge.")
        .trim_end_matches("()");
    let found: Option<(bool, String)> = sqlx::query_as(
        "SELECT pg_catalog.pg_get_userbyid(p.proowner) = 'forge_owner' AND NOT p.prosecdef \
         AND p.proconfig = ARRAY['search_path=pg_catalog, pg_temp'] \
         AND p.prolang = (SELECT l.oid FROM pg_catalog.pg_language l WHERE l.lanname = 'plpgsql'), \
         p.prosrc \
         FROM pg_catalog.pg_proc p WHERE p.oid = pg_catalog.to_regprocedure($1)",
    )
    .bind(signature)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.guard_function", e))?;
    let Some((shape, body)) = found else {
        return Err(guard_missing("function_owner").with_context("function", name));
    };
    if !shape || migration_body(name).as_deref() != Some(body.as_str()) {
        return Err(guard_missing("function_body").with_context("function", name));
    }
    Ok(())
}

/// The body of `forge.<name>()` in the last migration that defines it.
fn migration_body(name: &str) -> Option<String> {
    let marker = format!("FUNCTION forge.{name}()");
    crate::MIGRATOR.iter().rev().find_map(|m| {
        let sql = m.sql.as_ref();
        // A definition, not a trigger's `EXECUTE FUNCTION …`.
        let start = sql
            .rmatch_indices(marker.as_str())
            .map(|(i, _)| i)
            .find(|&i| {
                let before = sql.get(..i).unwrap_or_default();
                before.ends_with("CREATE ") || before.ends_with("CREATE OR REPLACE ")
            })?;
        let rest = sql.get(start..)?;
        let open = rest.find("AS $$")? + "AS $$".len();
        let body = rest.get(open..)?;
        let close = body.find("$$")?;
        body.get(..close).map(str::to_owned)
    })
}
