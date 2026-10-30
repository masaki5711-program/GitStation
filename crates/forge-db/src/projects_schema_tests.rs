//! The D4 schema (projects, repositories, project_members, protected_refs;
//! migration 20261031100000): the ledger, audit enforcement, privileges and
//! constraints. Audit probes commit as forge_app (where the deferred trigger
//! fires); constraint probes run as superuser in transactions that are rolled
//! back, so only the constraint under test can refuse a row.

use forge_core::RefPattern;
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;
use sqlx::postgres::PgConnection;
use sqlx::Connection as _;

use crate::audited_tests::{assert_refused, audit, commit, sql};
use crate::bootstrap::audited::{check_audited_triggers, AuditResource, Ledger};
use crate::test_support::*;

const PROJECT_TABLES: &[&str] = &[
    "projects",
    "repositories",
    "project_members",
    "protected_refs",
];

/// User 1 (human), user 2 (bot) and project 1, with their audit rows.
fn seed() -> Vec<String> {
    vec![
        audit("user", 1, "succeeded"),
        sql(
            "INSERT INTO forge.users (id, kind, username, display_name, email) \
             OVERRIDING SYSTEM VALUE VALUES (1, 'human', 'alice', 'A', 'alice@example.test')",
        ),
        audit("user", 2, "succeeded"),
        sql(
            "INSERT INTO forge.users (id, kind, username, display_name) \
             OVERRIDING SYSTEM VALUE VALUES (2, 'bot', 'ci-bot', 'CI')",
        ),
        audit("project", 1, "succeeded"),
        sql(
            "INSERT INTO forge.projects (id, display_name) OVERRIDING SYSTEM VALUE \
             VALUES (1, 'Forge')",
        ),
    ]
}

const REPOSITORY: &str = "INSERT INTO forge.repositories (project_id, kind) VALUES (1, 'system')";

/// The project tables are audited state tables on their project, the read
/// pool sees every column but `repositories.provision_token`, and doctor
/// accepts their triggers, owner and exact ACL.
#[tokio::test]
async fn every_project_table_is_in_the_ledger() {
    let t = FreshDb::migrated().await;
    let raw: toml::Table = include_str!("../tables.toml")
        .parse()
        .expect("tables.toml parses");
    let ledger = Ledger::embedded().expect("ledger");
    for table in PROJECT_TABLES {
        let entry = raw["tables"][*table].as_table().expect(table);
        assert_eq!(entry["guarded"].as_bool(), Some(false), "{table}");
        assert_eq!(entry["kind"].as_str(), Some("audited"), "{table}");
        let audited = ledger
            .audited
            .iter()
            .find(|a| a.name == *table)
            .expect(table);
        assert_eq!(
            audited.audit_resource,
            AuditResource::Literal("project".into()),
            "{table}"
        );
        let id = if *table == "projects" {
            "id"
        } else {
            "project_id"
        };
        assert_eq!(audited.audit_id_column, id, "{table}");
        assert!(audited.txn_audited_columns.is_empty(), "{table}");
        let ro: Vec<&str> = entry["ro_columns"]
            .as_array()
            .expect("ro_columns")
            .iter()
            .filter_map(toml::Value::as_str)
            .collect();
        if *table == "repositories" {
            assert!(
                !ro.contains(&"provision_token") && !ro.contains(&"*"),
                "{ro:?}"
            );
        } else {
            assert_eq!(ro, ["*"], "{table}");
        }
    }
    let mut su = t.superuser().await;
    check_audited_triggers(&mut su, &ledger)
        .await
        .expect("triggers match the ledger");
    crate::bootstrap::check(&t.db, &[])
        .await
        .expect("doctor accepts the D4 tables");
    drop(su);
    t.drop_now().await;
}

#[tokio::test]
async fn a_repository_state_change_needs_a_project_audit_row() {
    let t = FreshDb::migrated().await;
    commit(&t, &seed()).await.expect("seed");
    let err = commit(&t, &[sql(REPOSITORY)])
        .await
        .expect_err("no audit row");
    assert_refused(&err, "repositories");
    commit(&t, &[audit("project", 1, "succeeded"), sql(REPOSITORY)])
        .await
        .expect("created with its audit row");
    let ready = "UPDATE forge.repositories SET state = 'ready', ready_at = now()";
    let err = commit(&t, &[sql(ready)]).await.expect_err("no audit row");
    assert_refused(&err, "repositories");
    // Another project's audit row does not cover it, nor a failed one.
    let err = commit(&t, &[audit("project", 2, "succeeded"), sql(ready)])
        .await
        .expect_err("other project");
    assert_refused(&err, "repositories");
    let err = commit(&t, &[audit("project", 1, "failed"), sql(ready)])
        .await
        .expect_err("failed audit row");
    assert_refused(&err, "repositories");
    commit(&t, &[audit("project", 1, "succeeded"), sql(ready)])
        .await
        .expect("ready with its audit row");
    t.drop_now().await;
}

/// A repository moves only provisioning → ready, setting ready_at once; the
/// guard trigger repositories_state_forward refuses anything else with XF001
/// (FG-DB-0009 `rejected`), audit row or not.
#[tokio::test]
async fn a_repository_state_only_moves_forward() {
    let t = FreshDb::migrated().await;
    commit(&t, &seed()).await.expect("seed");
    commit(&t, &[audit("project", 1, "succeeded"), sql(REPOSITORY)])
        .await
        .expect("repository");
    // Staying in provisioning with a no-op update is fine.
    commit(
        &t,
        &[
            audit("project", 1, "succeeded"),
            sql("UPDATE forge.repositories SET state = 'provisioning'"),
        ],
    )
    .await
    .expect("no change");
    commit(
        &t,
        &[
            audit("project", 1, "succeeded"),
            sql("UPDATE forge.repositories SET state = 'ready', ready_at = now()"),
        ],
    )
    .await
    .expect("provisioning -> ready");
    for change in [
        "UPDATE forge.repositories SET state = 'provisioning', ready_at = NULL",
        "UPDATE forge.repositories SET ready_at = now() + interval '1 hour'",
        "UPDATE forge.repositories SET ready_at = NULL",
    ] {
        let err = commit(&t, &[audit("project", 1, "succeeded"), sql(change)])
            .await
            .expect_err(change);
        assert_reason(
            &err,
            forge_errors::ErrorCode::AppendOnlyViolation,
            "rejected",
        );
        let text = chain(&err);
        assert!(text.contains("sqlstate=XF001"), "{change}: {text}");
        assert!(text.contains("forge.repositories"), "{change}: {text}");
    }
    // A ready repository may be updated without touching state / ready_at.
    commit(
        &t,
        &[
            audit("project", 1, "succeeded"),
            sql("UPDATE forge.repositories SET state = 'ready'"),
        ],
    )
    .await
    .expect("no change");
    t.drop_now().await;
}

/// Doctor compares the state guard's trigger and function body like the
/// other guards (FG-DB-0009 guard_missing).
#[tokio::test]
async fn a_weakened_repository_state_guard_is_reported() {
    for (tamper, guard) in [
        (
            "ALTER TABLE forge.repositories ENABLE TRIGGER repositories_state_forward",
            "guard=log_triggers",
        ),
        (
            "ALTER TABLE forge.repositories DISABLE TRIGGER repositories_state_forward",
            "guard=log_triggers",
        ),
        (
            "DROP TRIGGER repositories_state_forward ON forge.repositories",
            "guard=log_triggers",
        ),
        (
            "CREATE OR REPLACE FUNCTION forge.repositories_state_guard() RETURNS trigger \
             LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp \
             AS $$ BEGIN RETURN NEW; END $$",
            "guard=function_body",
        ),
    ] {
        let t = FreshDb::migrated().await;
        t.exec_superuser(tamper).await;
        let text = crate::bootstrap::check(&t.db, &[])
            .await
            .expect_err(tamper)
            .to_string();
        assert!(text.contains(guard), "{tamper}\n=> {text}");
        t.drop_now().await;
    }
}

#[tokio::test]
async fn repository_kind_cannot_change() {
    let t = FreshDb::migrated().await;
    commit(&t, &seed()).await.expect("seed");
    commit(&t, &[audit("project", 1, "succeeded"), sql(REPOSITORY)])
        .await
        .expect("repository");
    // forge_app may update state and ready_at only, and delete nothing.
    for change in [
        "UPDATE forge.repositories SET kind = 'spec'",
        "UPDATE forge.repositories SET project_id = 1",
        "UPDATE forge.repositories SET provision_token = gen_random_uuid()",
        "UPDATE forge.repositories SET created_at = now()",
        "DELETE FROM forge.repositories",
    ] {
        let err = commit(&t, &[audit("project", 1, "succeeded"), sql(change)])
            .await
            .expect_err(change);
        assert!(chain(&err).contains("permission denied"), "{change}: {err}");
    }
    let kind: String = sqlx::query_scalar("SELECT kind FROM forge.repositories")
        .fetch_one(t.db.write_pool())
        .await
        .expect("kind");
    assert_eq!(kind, "system");
    t.drop_now().await;
}

#[tokio::test]
async fn the_read_pool_cannot_read_provision_token() {
    let t = FreshDb::migrated().await;
    commit(&t, &seed()).await.expect("seed");
    commit(&t, &[audit("project", 1, "succeeded"), sql(REPOSITORY)])
        .await
        .expect("repository");
    let ro = t.db.pool().executor();
    for query in [
        "SELECT provision_token FROM forge.repositories",
        "SELECT * FROM forge.repositories",
        "SELECT id FROM forge.repositories WHERE provision_token IS NOT NULL",
    ] {
        let err = sqlx::query(query).fetch_all(ro).await.expect_err(query);
        assert!(
            err.to_string().contains("permission denied"),
            "{query}: {err}"
        );
    }
    let rows = sqlx::query(
        "SELECT id, project_id, kind, state, created_at, ready_at FROM forge.repositories",
    )
    .fetch_all(ro)
    .await
    .expect("the other columns are readable");
    assert_eq!(rows.len(), 1);
    for table in ["projects", "project_members", "protected_refs"] {
        sqlx::query(&format!("SELECT * FROM forge.{table}"))
            .fetch_all(ro)
            .await
            .unwrap_or_else(|e| panic!("{table}: {e}"));
    }
    // The application reads the token (it writes it into the bare repository).
    let token: String = sqlx::query_scalar("SELECT provision_token::text FROM forge.repositories")
        .fetch_one(t.db.write_pool())
        .await
        .expect("forge_app reads the token");
    assert_eq!(token.len(), 36, "a generated uuid: {token}");
    t.drop_now().await;
}

#[tokio::test]
async fn a_project_table_change_without_its_audit_row_cannot_commit() {
    let t = FreshDb::migrated().await;
    commit(&t, &seed()).await.expect("seed");
    let member =
        sql("INSERT INTO forge.project_members (project_id, user_id, role) VALUES (1, 1, 'owner')");
    let rule = sql(
        "INSERT INTO forge.protected_refs (project_id, pattern, push_min_role) \
                    VALUES (1, 'refs/heads/main', 'maintainer')",
    );
    for (add, table) in [(&member, "project_members"), (&rule, "protected_refs")] {
        let err = commit(&t, std::slice::from_ref(add))
            .await
            .expect_err("no audit row");
        assert_refused(&err, table);
        let err = commit(&t, &[audit("project", 2, "succeeded"), add.clone()])
            .await
            .expect_err("other project");
        assert_refused(&err, table);
        commit(&t, &[audit("project", 1, "succeeded"), add.clone()])
            .await
            .expect("with its audit row");
    }
    for (change, table) in [
        (
            "UPDATE forge.projects SET display_name = 'Other'",
            "projects",
        ),
        (
            "UPDATE forge.projects SET visibility = 'internal'",
            "projects",
        ),
        (
            "UPDATE forge.projects SET deleted_at = now(), purge_after = now() + interval '7 days'",
            "projects",
        ),
        (
            "UPDATE forge.project_members SET role = 'guest'",
            "project_members",
        ),
        (
            "UPDATE forge.protected_refs SET push_min_role = 'owner'",
            "protected_refs",
        ),
        (
            "UPDATE forge.protected_refs SET pattern = 'refs/heads/*'",
            "protected_refs",
        ),
        ("DELETE FROM forge.protected_refs", "protected_refs"),
        ("DELETE FROM forge.project_members", "project_members"),
    ] {
        let err = commit(&t, &[sql(change)]).await.expect_err(change);
        assert_refused(&err, table);
    }
    for unaudited in [
        "UPDATE forge.projects SET updated_at = now()",
        "UPDATE forge.project_members SET updated_at = now()",
        "UPDATE forge.protected_refs SET updated_at = now()",
    ] {
        commit(&t, &[sql(unaudited)]).await.expect(unaudited);
    }
    // forge_app may remove members and rules (with the audit row), never a
    // project (D5 adds purge).
    commit(
        &t,
        &[
            audit("project", 1, "succeeded"),
            sql("DELETE FROM forge.protected_refs"),
            sql("DELETE FROM forge.project_members"),
        ],
    )
    .await
    .expect("delete with the audit row");
    let err = commit(
        &t,
        &[
            audit("project", 1, "succeeded"),
            sql("DELETE FROM forge.projects"),
        ],
    )
    .await
    .expect_err("no DELETE on projects");
    assert!(chain(&err).contains("permission denied"), "{err}");
    t.drop_now().await;
}

/// Run `setup`, then `probe`, as superuser in a transaction that is rolled
/// back: the SQLSTATE and constraint name of the probe's failure.
async fn refusal(su: &mut PgConnection, setup: &str, probe: &str) -> Option<(String, String)> {
    let mut tx = su.begin().await.expect("begin");
    if !setup.is_empty() {
        sqlx::raw_sql(setup).execute(&mut *tx).await.expect(setup);
    }
    let result = sqlx::raw_sql(probe).execute(&mut *tx).await;
    tx.rollback().await.expect("rollback");
    let e = result.err()?;
    let db = e.as_database_error().expect("a database error");
    Some((
        db.code().map(|c| c.into_owned()).unwrap_or_default(),
        db.constraint().unwrap_or_default().to_owned(),
    ))
}

#[tokio::test]
async fn the_d4_constraints_refuse_their_rows() {
    let t = FreshDb::migrated().await;
    commit(&t, &seed()).await.expect("seed");
    let mut su = t.superuser().await;
    let repo = |cols: &str, vals: &str| {
        format!(
            "INSERT INTO forge.repositories (project_id, kind{cols}) VALUES (1, 'system'{vals})"
        )
    };
    let rule = |pattern: &str, role: &str| {
        format!(
            "INSERT INTO forge.protected_refs (project_id, pattern, push_min_role) \
             VALUES (1, '{pattern}', '{role}')"
        )
    };
    let member = |user: i64, role: &str| {
        format!(
            "INSERT INTO forge.project_members (project_id, user_id, role) \
             VALUES (1, {user}, '{role}')"
        )
    };
    let long = format!("refs/heads/{}", "a".repeat(245));
    let main_rule = rule("refs/heads/Main", "owner");
    let owner = member(1, "owner");
    let cases: Vec<(&str, String, String, &str, &str)> = vec![
        ("", "INSERT INTO forge.projects (display_name) VALUES ('')".into(), "23514".into(), "projects_display_name_length", "empty name"),
        ("", "INSERT INTO forge.projects (display_name, description) VALUES ('A', repeat('x', 2001))".into(), "23514".into(), "projects_description_length", "long description"),
        ("", "INSERT INTO forge.projects (display_name, visibility) VALUES ('A', 'public')".into(), "23514".into(), "projects_visibility", "public"),
        ("", "INSERT INTO forge.projects (display_name, deleted_at) VALUES ('A', now())".into(), "23514".into(), "projects_deletion_pair", "deleted without purge"),
        ("", "INSERT INTO forge.projects (display_name, purge_after) VALUES ('A', now())".into(), "23514".into(), "projects_deletion_pair", "purge without deleted"),
        ("", "INSERT INTO forge.projects (display_name, deleted_at, purge_after) VALUES ('A', now(), now() - interval '1 second')".into(), "23514".into(), "projects_deletion_pair", "purge before delete"),
        ("", repo("", "").replace("'system'", "'docs'"), "23514".into(), "repositories_kind", "unknown kind"),
        ("", repo(", state", ", 'deleted'"), "23514".into(), "repositories_state", "unknown state"),
        ("", repo(", state", ", 'ready'"), "23514".into(), "repositories_ready_at", "ready without ready_at"),
        ("", repo(", ready_at", ", now()"), "23514".into(), "repositories_ready_at", "provisioning with ready_at"),
        ("", repo(", provision_token", ", NULL"), "23502".into(), "", "no token"),
        ("", repo("", "").replace("(1,", "(9,"), "23503".into(), "repositories_project_id_fkey", "unknown project"),
        (REPOSITORY, repo("", ""), "23505".into(), "repositories_project_key", "two repositories"),
        ("", rule("main", "maintainer"), "23514".into(), "protected_refs_pattern_prefix", "no prefix"),
        ("", rule("refs/remotes/x", "maintainer"), "23514".into(), "protected_refs_pattern_prefix", "remotes"),
        ("", rule(&long, "maintainer"), "23514".into(), "protected_refs_pattern_length", "too long"),
        ("", rule("refs/heads/main", "reporter"), "23514".into(), "protected_refs_push_min_role", "reporter"),
        ("", rule("refs/heads/main", "guest"), "23514".into(), "protected_refs_push_min_role", "guest"),
        (main_rule.as_str(), rule("refs/heads/mAIN", "developer"), "23505".into(), "protected_refs_project_pattern", "same pattern ignoring case"),
        ("", rule("refs/heads/main", "owner").replace("(1,", "(9,"), "23503".into(), "protected_refs_project_id_fkey", "unknown project"),
        ("", member(1, "admin"), "23514".into(), "project_members_role", "unknown role"),
        ("", member(9, "owner"), "23503".into(), "project_members_user_id_fkey", "unknown user"),
        ("", member(2, "guest"), "23514".into(), "project_members_no_bot", "a bot member"),
        (owner.as_str(), member(1, "guest"), "23505".into(), "project_members_pkey", "one row per member"),
        (owner.as_str(), "UPDATE forge.project_members SET user_id = 2".into(), "23514".into(), "project_members_no_bot", "a member changed into a bot"),
        // The state guard holds for every role (ENABLE ALWAYS), not only for
        // forge_app's column grants.
        (REPOSITORY, "UPDATE forge.repositories SET state = 'ready', ready_at = now(); UPDATE forge.repositories SET state = 'provisioning', ready_at = NULL".into(), "XF001".into(), "", "ready back to provisioning (owner)"),
        (REPOSITORY, "UPDATE forge.repositories SET state = 'ready', ready_at = now(); UPDATE forge.repositories SET ready_at = ready_at - interval '1 day'".into(), "XF001".into(), "", "ready_at rewritten (owner)"),
    ];
    for (setup, probe, state, constraint, what) in &cases {
        let got = refusal(&mut su, setup, probe).await;
        assert_eq!(
            got,
            Some((state.clone(), (*constraint).to_owned())),
            "{what}"
        );
    }
    // The longest pattern, and both prefixes, are accepted.
    let longest = format!("refs/heads/{}", "a".repeat(244));
    for ok in [
        rule(&longest, "owner"),
        rule("refs/tags/*", "developer"),
        repo(", state, ready_at", ", 'ready', now()"),
        "INSERT INTO forge.projects (display_name, deleted_at, purge_after) \
         VALUES ('A', now(), now() + interval '7 days')"
            .to_owned(),
    ] {
        assert_eq!(refusal(&mut su, "", &ok).await, None, "{ok}");
    }
    drop(su);
    t.drop_now().await;
}

/// Characters that make interesting patterns (no NUL: the server refuses it
/// in any text before a CHECK runs).
fn pattern() -> impl Strategy<Value = String> {
    let prefix = prop::sample::select(vec![
        "refs/heads/",
        "refs/tags/",
        "refs/",
        "refs/remotes/",
        "REFS/HEADS/",
        "refs/heads",
        "",
    ]);
    let tail = proptest::collection::vec(
        prop_oneof![
            4 => prop::sample::select(vec!['a', 'Z', '0']),
            1 => prop::sample::select(vec!['/', '*', '.', '-', '~', ' ', '\u{7f}', 'é', '\'']),
        ],
        0..300,
    );
    (prefix, tail).prop_map(|(p, cs)| format!("{p}{}", cs.into_iter().collect::<String>()))
}

/// Property test: where the parser and the database overlap (the prefix and
/// the length), they agree — the database refuses a pattern exactly when
/// RefPattern::parse reports `prefix` or `too_long`. The other rules
/// (wildcard, check-ref-format) are the parser's alone. The cases come from a
/// deterministic proptest runner; each is inserted for real (savepoint,
/// rolled back).
#[tokio::test]
async fn the_db_check_agrees_with_the_ref_pattern_parser() {
    let t = FreshDb::migrated().await;
    commit(&t, &seed()).await.expect("seed");
    let mut runner = TestRunner::deterministic();
    let strategy = pattern();
    let mut cases: Vec<String> = (0..512)
        .map(|_| strategy.new_tree(&mut runner).expect("case").current())
        .collect();
    cases.extend(
        [
            "refs/heads/",
            "refs/tags/x",
            "refs/heads/**",
            "refs/heads/a..b",
        ]
        .map(str::to_owned),
    );
    cases.push(format!("refs/heads/{}", "é".repeat(122))); // 255 bytes
    cases.push(format!("refs/heads/{}a", "é".repeat(122))); // 256 bytes, 134 chars
    let mut su = t.superuser().await;
    let mut tx = su.begin().await.expect("begin");
    for raw in &cases {
        sqlx::raw_sql("SAVEPOINT probe")
            .execute(&mut *tx)
            .await
            .expect("savepoint");
        let db = sqlx::query(
            "INSERT INTO forge.protected_refs (project_id, pattern, push_min_role) \
             VALUES (1, $1, 'owner')",
        )
        .bind(raw)
        .execute(&mut *tx)
        .await;
        sqlx::raw_sql("ROLLBACK TO SAVEPOINT probe")
            .execute(&mut *tx)
            .await
            .expect("rollback to savepoint");
        let db_refuses = match db {
            Ok(_) => false,
            Err(e) => {
                let code = e
                    .as_database_error()
                    .and_then(|d| d.code())
                    .map(|c| c.into_owned());
                assert_eq!(code.as_deref(), Some("23514"), "{raw:?}: {e}");
                true
            }
        };
        let parser = RefPattern::parse(raw);
        let parser_says_db_refuses = parser.as_ref().err().is_some_and(|e| {
            e.context()
                .iter()
                .any(|(k, v)| *k == "reason" && (v == "prefix" || v == "too_long"))
        });
        assert_eq!(db_refuses, parser_says_db_refuses, "{raw:?}: {parser:?}");
    }
    tx.rollback().await.expect("rollback");
    drop(su);
    t.drop_now().await;
}
