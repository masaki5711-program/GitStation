//! Protected ref rules against a migrated database (D4 MR5). Inspection uses
//! a superuser connection.

use forge_core::{EventType, RequestId, VisibilityClass};
use serde_json::json;

use super::*;
use crate::audited_tests::{audit, commit as commit_sql};
use crate::change::Actor;
use crate::projects::{self, CreatedProject, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

fn pattern(raw: &str) -> RefPattern {
    RefPattern::parse(raw).unwrap_or_else(|e| panic!("{raw}: {e}"))
}

fn ref_name(raw: &str) -> RefName {
    RefName::parse(raw).unwrap_or_else(|e| panic!("{raw}: {e}"))
}

/// `alice/<segment>`, created by alice.
async fn project_of_alice(t: &FreshDb, segments: &[&str]) -> Vec<CreatedProject> {
    let alice = users::create(
        &t.db,
        &ctx("req-user-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let mut out = Vec::new();
    for segment in segments {
        let created = projects::create(
            &t.db,
            &ctx(&format!("req-project-{segment}")),
            &NewProject::new(segment, alice.namespace_id, segment, alice.user_id),
        )
        .await
        .expect("project");
        out.push(created);
    }
    out
}

async fn web(t: &FreshDb) -> ProjectId {
    project_of_alice(t, &["web"]).await[0].project_id
}

async fn create_ok(t: &FreshDb, p: ProjectId, raw: &str, role: Role) -> ProtectedRefId {
    let request = format!("req-rule-{}", raw.len());
    create(&t.db, &ctx(&request), p, &pattern(raw), role)
        .await
        .unwrap_or_else(|e| panic!("{raw}: {e}"))
}

async fn rule_count(t: &FreshDb, p: ProjectId) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM forge.protected_refs WHERE project_id = $1")
        .bind(p.get())
        .fetch_one(&mut t.superuser().await)
        .await
        .expect("count")
}

/// (action, resource_type, resource_id, project_id) of a request's audit rows.
async fn audit_rows(t: &FreshDb, request: &str) -> Vec<(String, String, i64, i64)> {
    sqlx::query_as(
        "SELECT action, resource_type, resource_id, project_id FROM forge.audit_log \
         WHERE request_id = $1 ORDER BY id",
    )
    .bind(request)
    .fetch_all(&mut t.superuser().await)
    .await
    .expect("audit rows")
}

/// (event_type, aggregate_type, aggregate_id, project_id, payload, has a
/// dedup key) of a request's events.
type EventRow = (String, String, i64, i64, serde_json::Value, bool);

async fn events_of(t: &FreshDb, request: &str) -> Vec<EventRow> {
    sqlx::query_as(
        "SELECT event_type, aggregate_type, aggregate_id, project_id, payload, \
           dedup_key IS NOT NULL FROM forge.events WHERE request_id = $1 ORDER BY id",
    )
    .bind(request)
    .fetch_all(&mut t.superuser().await)
    .await
    .expect("events")
}

#[tokio::test]
async fn protected_ref_change_writes_one_audit_row_and_one_maintainer_event() {
    let t = FreshDb::migrated().await;
    let p = web(&t).await;
    let rule = create(
        &t.db,
        &ctx("req-create"),
        p,
        &pattern("refs/heads/release/*"),
        Role::Maintainer,
    )
    .await
    .expect("create");
    let changed = change(&t.db, &ctx("req-change"), p, rule, Role::Owner)
        .await
        .expect("change");
    assert_eq!(
        changed,
        RuleChange::Changed {
            from: Role::Maintainer
        }
    );
    let same = change(&t.db, &ctx("req-same"), p, rule, Role::Owner)
        .await
        .expect("same");
    assert_eq!(same, RuleChange::Unchanged);
    delete(&t.db, &ctx("req-delete"), p, rule)
        .await
        .expect("delete");
    let id = rule.get();
    for (request, action, details) in [
        (
            "req-create",
            "protected_ref.created",
            json!({"protected_ref_id": id, "pattern": "refs/heads/release/*",
                   "push_min_role": "maintainer"}),
        ),
        (
            "req-change",
            "protected_ref.updated",
            json!({"protected_ref_id": id, "pattern": "refs/heads/release/*",
                   "from": "maintainer", "to": "owner"}),
        ),
        (
            "req-delete",
            "protected_ref.deleted",
            json!({"protected_ref_id": id, "pattern": "refs/heads/release/*",
                   "push_min_role": "owner"}),
        ),
    ] {
        assert_eq!(
            audit_rows(&t, request).await,
            vec![(action.to_owned(), "project".to_owned(), p.get(), p.get())],
            "{request}"
        );
        let stored: serde_json::Value =
            sqlx::query_scalar("SELECT details FROM forge.audit_log WHERE request_id = $1")
                .bind(request)
                .fetch_one(&mut t.superuser().await)
                .await
                .expect("details");
        assert_eq!(stored, details, "{request}");
        assert_eq!(
            events_of(&t, request).await,
            vec![(
                action.to_owned(),
                "project".to_owned(),
                p.get(),
                p.get(),
                details,
                false
            )],
            "{request}"
        );
        let class = EventType::parse(action).map(EventType::visibility_class);
        assert_eq!(class, Some(VisibilityClass::Maintainer), "{action}");
    }
    // Unchanged wrote nothing.
    assert!(audit_rows(&t, "req-same").await.is_empty());
    assert!(events_of(&t, "req-same").await.is_empty());
    assert_eq!(rule_count(&t, p).await, 0);
    t.drop_now().await;
}

#[tokio::test]
async fn levels_for_returns_every_matching_rule_case_insensitively() {
    let t = FreshDb::migrated().await;
    let projects = project_of_alice(&t, &["web", "api"]).await;
    let (p, other) = (projects[0].project_id, projects[1].project_id);
    create_ok(&t, p, "refs/heads/main", Role::Developer).await;
    create_ok(&t, p, "refs/heads/*", Role::Maintainer).await;
    create_ok(&t, p, "refs/tags/v*", Role::Owner).await;
    // Non-ASCII: a word-final capital sigma, and the dotted capital I.
    create_ok(&t, p, "refs/heads/ΟΔΟΣ", Role::Owner).await;
    create_ok(&t, p, "refs/heads/İnfo", Role::Owner).await;
    // Another project's rule never applies here.
    create_ok(&t, other, "refs/heads/MAIN", Role::Owner).await;
    let levels = |raw: &'static str| {
        let db = &t.db;
        async move {
            levels_for(db, p, &ref_name(raw))
                .await
                .unwrap_or_else(|e| panic!("{raw}: {e}"))
        }
    };
    // Every matching rule, in rule order (the exact rule and the wildcard).
    assert_eq!(
        levels("refs/heads/main").await,
        [Role::Developer, Role::Maintainer]
    );
    assert_eq!(
        levels("refs/heads/MAIN").await,
        [Role::Developer, Role::Maintainer]
    );
    assert_eq!(levels("refs/heads/feature/x").await, [Role::Maintainer]);
    assert_eq!(levels("refs/tags/V1.0").await, [Role::Owner]);
    assert!(levels("refs/tags/release").await.is_empty());
    assert!(levels("refs/notes/main").await.is_empty());
    // Σ, σ and the final ς fold to one letter wherever they stand.
    for raw in ["refs/heads/οδος", "refs/heads/ΟΔΟΣ", "refs/heads/οδοσ"] {
        assert_eq!(levels(raw).await, [Role::Maintainer, Role::Owner], "{raw}");
    }
    // İ folds to i + U+0307: that spelling matches, a plain i does not.
    assert_eq!(
        levels("refs/heads/i\u{307}nfo").await,
        [Role::Maintainer, Role::Owner]
    );
    assert_eq!(levels("refs/heads/info").await, [Role::Maintainer]);
    // A project without rules protects nothing.
    assert!(
        levels_for(&t.db, ProjectId::new(9_999), &ref_name("refs/heads/main"))
            .await
            .expect("no rules")
            .is_empty()
    );
    t.drop_now().await;
}

#[tokio::test]
async fn duplicate_pattern_ignoring_case_is_rejected() {
    let t = FreshDb::migrated().await;
    let projects = project_of_alice(&t, &["web", "api"]).await;
    let (p, other) = (projects[0].project_id, projects[1].project_id);
    for (first, second) in [
        // ASCII: the unique index would also catch it.
        ("refs/heads/main", "refs/heads/MAIN"),
        // Non-ASCII: only RefPattern::key() sees these (lower() under
        // COLLATE "C" folds ASCII only).
        ("refs/heads/ärger", "refs/heads/ÄRGER"),
        ("refs/heads/οδος", "refs/heads/ΟΔΟΣ"),
        ("refs/tags/İ*", "refs/tags/i\u{307}*"),
    ] {
        create_ok(&t, p, first, Role::Developer).await;
        let request = format!("req-dup-{}", second.len());
        let err = create(&t.db, &ctx(&request), p, &pattern(second), Role::Owner)
            .await
            .expect_err(second);
        assert_reason(
            &err,
            ErrorCode::ProtectedRefRuleInvalid,
            "duplicate_pattern",
        );
        assert_eq!(err.code().http_status(), 400);
        assert!(
            !err.to_string().contains(second),
            "the pattern is not logged"
        );
        assert!(audit_rows(&t, &request).await.is_empty(), "{second}");
    }
    assert_eq!(rule_count(&t, p).await, 4);
    // The same pattern in another project is fine.
    create_ok(&t, other, "refs/heads/MAIN", Role::Owner).await;
    t.drop_now().await;
}

#[tokio::test]
async fn more_than_100_rules_is_rejected() {
    let t = FreshDb::migrated().await;
    let p = web(&t).await;
    for n in 0..MAX_RULES {
        create(
            &t.db,
            &ctx(&format!("req-rule-{n}")),
            p,
            &pattern(&format!("refs/heads/r{n}")),
            Role::Developer,
        )
        .await
        .unwrap_or_else(|e| panic!("rule {n}: {e}"));
    }
    let err = create(
        &t.db,
        &ctx("req-rule-101"),
        p,
        &pattern("refs/heads/one-too-many"),
        Role::Developer,
    )
    .await
    .expect_err("the 101st rule");
    assert_reason(&err, ErrorCode::ProtectedRefRuleInvalid, "too_many_rules");
    assert!(audit_rows(&t, "req-rule-101").await.is_empty());
    assert_eq!(rule_count(&t, p).await, 100);
    // levels_for reads all 100.
    assert_eq!(
        levels_for(&t.db, p, &ref_name("refs/heads/r99"))
            .await
            .expect("levels"),
        [Role::Developer]
    );
    // A 101st row written outside forge-db: levels_for refuses rather than
    // deciding on a partial rule set.
    commit_sql(
        &t,
        &[
            audit("project", p.get(), "succeeded"),
            format!(
                "INSERT INTO forge.protected_refs (project_id, pattern, push_min_role) \
                 VALUES ({}, 'refs/heads/smuggled', 'owner')",
                p.get()
            ),
        ],
    )
    .await
    .expect("smuggle a rule");
    let err = levels_for(&t.db, p, &ref_name("refs/heads/main"))
        .await
        .expect_err("over the cap");
    assert_reason(
        &err,
        ErrorCode::ProtectedRefDataInvalid,
        "too_many_stored_rules",
    );
    t.drop_now().await;
}

#[tokio::test]
async fn invalid_rule_requests_and_unknown_rules_are_refused() {
    let t = FreshDb::migrated().await;
    let projects = project_of_alice(&t, &["web", "api"]).await;
    let (p, other) = (projects[0].project_id, projects[1].project_id);
    for role in [Role::Guest, Role::Reporter] {
        let err = create(&t.db, &ctx("req-guest"), p, &pattern("refs/heads/x"), role)
            .await
            .expect_err("guest / reporter protect nothing");
        assert_reason(&err, ErrorCode::ProtectedRefRuleInvalid, "push_min_role");
    }
    let rule = create_ok(&t, p, "refs/heads/main", Role::Developer).await;
    let err = change(&t.db, &ctx("req-to-guest"), p, rule, Role::Guest)
        .await
        .expect_err("guest");
    assert_reason(&err, ErrorCode::ProtectedRefRuleInvalid, "push_min_role");
    // A rule is addressed within its project.
    for err in [
        change(&t.db, &ctx("req-other"), other, rule, Role::Owner)
            .await
            .expect_err("another project's rule"),
        delete(&t.db, &ctx("req-other-del"), other, rule)
            .await
            .expect_err("another project's rule"),
        delete(&t.db, &ctx("req-absent"), p, ProtectedRefId::new(9_999))
            .await
            .expect_err("absent"),
    ] {
        assert_reason(&err, ErrorCode::NotFound, "absent");
        assert!(err.to_string().contains("resource=protected_ref"), "{err}");
    }
    let err = create(
        &t.db,
        &ctx("req-noproject"),
        ProjectId::new(9_999),
        &pattern("refs/heads/main"),
        Role::Owner,
    )
    .await
    .expect_err("unknown project");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    assert!(err.to_string().contains("resource=project"), "{err}");
    assert_eq!(rule_count(&t, p).await, 1);
    t.drop_now().await;
}

/// Two creates of one project at once: the project lock serializes them, so
/// two patterns equal ignoring non-ASCII case (which the unique index cannot
/// see) never both commit.
#[tokio::test]
async fn two_concurrent_duplicate_creates_make_one_rule() {
    let t = FreshDb::migrated().await;
    let p = web(&t).await;
    let (a, b) = (pattern("refs/heads/ärger"), pattern("refs/heads/ÄRGER"));
    let (ctx_a, ctx_b) = (ctx("req-a"), ctx("req-b"));
    let (ra, rb) = tokio::join!(
        create(&t.db, &ctx_a, p, &a, Role::Developer),
        create(&t.db, &ctx_b, p, &b, Role::Developer),
    );
    let refused: Vec<_> = [ra, rb].into_iter().filter_map(Result::err).collect();
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_reason(
        &refused[0],
        ErrorCode::ProtectedRefRuleInvalid,
        "duplicate_pattern",
    );
    assert_eq!(rule_count(&t, p).await, 1);
    t.drop_now().await;
}

/// A stored pattern or role the parser refuses (written outside forge-db,
/// past the database CHECKs that hold only the prefix and the length) is
/// `FG-DB-0020`: never skipped, never matched loosely.
#[tokio::test]
async fn a_stored_rule_this_build_cannot_read_is_protected_ref_data_invalid() {
    let t = FreshDb::migrated().await;
    let p = web(&t).await;
    let rule = create_ok(&t, p, "refs/heads/main", Role::Maintainer).await;
    // A pattern with two wildcards passes the CHECKs (prefix, length).
    commit_sql(
        &t,
        &[
            audit("project", p.get(), "succeeded"),
            format!(
                "UPDATE forge.protected_refs SET pattern = 'refs/heads/*/*' WHERE id = {}",
                rule.get()
            ),
        ],
    )
    .await
    .expect("tamper the pattern");
    let main = ref_name("refs/heads/main");
    for err in [
        levels_for(&t.db, p, &main).await.expect_err("levels"),
        create(
            &t.db,
            &ctx("req-next"),
            p,
            &pattern("refs/heads/x"),
            Role::Owner,
        )
        .await
        .expect_err("create reads the rules"),
        change(&t.db, &ctx("req-change"), p, rule, Role::Owner)
            .await
            .expect_err("change"),
    ] {
        assert_reason(
            &err,
            ErrorCode::ProtectedRefDataInvalid,
            "stored_pattern_invalid",
        );
        let text = err.to_string();
        assert!(
            text.contains(&format!("protected_ref_id={}", rule.get())),
            "{text}"
        );
        assert!(
            !text.contains("refs/heads/*/*"),
            "the pattern is not logged: {text}"
        );
    }
    // A role outside the three, with the CHECK dropped (this database is
    // dropped after the test).
    t.exec_superuser(&format!(
        "ALTER TABLE forge.protected_refs DROP CONSTRAINT protected_refs_push_min_role; \
         ALTER TABLE forge.protected_refs DISABLE TRIGGER USER; \
         UPDATE forge.protected_refs SET pattern = 'refs/heads/main', push_min_role = 'guest' \
           WHERE id = {}; \
         ALTER TABLE forge.protected_refs ENABLE TRIGGER USER",
        rule.get()
    ))
    .await;
    let err = levels_for(&t.db, p, &main).await.expect_err("levels");
    assert_reason(
        &err,
        ErrorCode::ProtectedRefDataInvalid,
        "stored_push_min_role_invalid",
    );
    t.drop_now().await;
}
