//! Acting for a person against a migrated database (M3 S2, ADR-0025 §1,
//! §5.1–§5.3): the person a header names, the assertions' states and
//! their secret (never stored), the MAC (a row written or changed without
//! the key never verifies), single use, the employee number guard.
//! Synthetic data only.

use std::time::Duration;

use forge_core::act_as::{ActAsKeys, ActAsTarget, AssertionToken, Intent};
use forge_core::ids::{IntegrationId, ProjectId, UserId};
use forge_core::{IntegrationKind, ObjectId, RefName, RequestId, Role};
use forge_errors::ErrorCode;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::integrations::{self, NewIntegration};
use crate::projects::{self, NewProject};
use crate::review::{self, NewMergeRequest, StoredMergeRequest};
use crate::service_accounts::{self, NewServiceAccount};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

fn as_user(user: UserId, request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::User(user),
        RequestId::parse(request).expect("request id"),
    )
}

fn as_sa(sa: UserId, request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::ServiceAccount(sa),
        RequestId::parse(request).expect("request id"),
    )
}

fn oid(c: char) -> ObjectId {
    ObjectId::parse(&c.to_string().repeat(40)).expect("oid")
}

/// A ring of one active key (kid 1): the 32 bytes 7, 8, 9, … or 8, 9, 10, ….
fn keys(byte: u8) -> ActAsKeys {
    let b64 = match byte {
        7 => "BwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAhIiMkJSY=",
        _ => "CAkKCwwNDg8QERITFBUWFxgZGhscHR4fICEiIyQlJic=",
    };
    ActAsKeys::parse(&format!("1:{b64} active")).expect("keys")
}

const STATE: &str = "s-0123456789abcdefghijklmnopqrstuv";

struct World {
    t: FreshDb,
    bob: UserId,
    sa: UserId,
    integration: IntegrationId,
    project: ProjectId,
    mr: StoredMergeRequest,
    keys: ActAsKeys,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &system("req-user-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let mut bob_new = NewUser::human("bob", "bob", "bob@example.test");
    bob_new.employee_number = Some("E-2001".to_owned());
    let bob = users::create(&t.db, &system("req-user-bob"), &bob_new)
        .await
        .expect("bob");
    let sa = service_accounts::create(
        &t.db,
        &system("req-sa"),
        &NewServiceAccount {
            username: "kanflow-sync".to_owned(),
            display_name: "kanflow".to_owned(),
            owner: alice.user_id,
            purpose: "kanflow acts for people who confirmed in forge".to_owned(),
        },
        alice.user_id,
    )
    .await
    .expect("sa")
    .user_id;
    let integration = integrations::create(
        &t.db,
        &system("req-int"),
        &NewIntegration {
            kind: IntegrationKind::Kanflow,
            service_account: sa,
            event_types: None,
        },
        alice.user_id,
    )
    .await
    .expect("integration");
    let project = projects::create(
        &t.db,
        &system("req-project"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("project")
    .project_id;
    projects::add_member(
        &t.db,
        &system("req-bob"),
        project,
        bob.user_id,
        Role::Developer,
    )
    .await
    .expect("bob is a developer");
    let (bound, _, ()) = integrations::bind_decided(
        &t.db,
        &system("req-bind"),
        project,
        alice.user_id,
        integration,
        "KF-1",
        |_| Ok(()),
    )
    .await
    .expect("bind");
    assert!(matches!(bound, integrations::BindOutcome::Bound { .. }));
    let (mr, ()) = review::open_mr_decided(
        &t.db,
        &as_user(alice.user_id, "req-open"),
        project,
        alice.user_id,
        &NewMergeRequest {
            title: "Feature".to_owned(),
            description: String::new(),
            source_branch: RefName::parse("refs/heads/feature").expect("branch"),
            target_branch: RefName::parse("refs/heads/main").expect("branch"),
            draft: false,
        },
        |_| Ok(()),
    )
    .await
    .expect("open");
    review::record_mr_version(&t.db, &system("req-v1"), mr.id, &oid('a'), None)
        .await
        .expect("v1");
    World {
        t,
        bob: bob.user_id,
        sa,
        integration,
        project,
        mr,
        keys: keys(7),
    }
}

impl World {
    fn main(&self) -> RefName {
        RefName::parse("refs/heads/main").expect("main")
    }

    async fn issue_for(&self, state: &str, lifetime: Duration) -> i64 {
        let head = oid('a');
        let target = self.main();
        let tip = oid('b');
        issue(
            &self.t.db,
            &as_user(self.bob, "req-issue"),
            &NewAssertion {
                integration: self.integration,
                user: self.bob,
                intent: Intent::Approve,
                project: self.project,
                subject: NewSubject::MergeRequest {
                    merge_request: self.mr.id,
                    head_sha: head.as_str(),
                    target_ref: &target,
                    target_sha: tip.as_str(),
                },
                state,
                lifetime,
            },
        )
        .await
        .expect("issue")
    }

    async fn claim_it(&self, state: &str, request: &str) -> Result<Claimed, ForgeError> {
        claim(
            &self.t.db,
            &as_sa(self.sa, request),
            &self.keys,
            self.integration,
            state,
        )
        .await
    }

    async fn verify(&self, token: &AssertionToken) -> Result<VerifiedAssertion, ForgeError> {
        verify_assertion(&self.t.db, &self.keys, token, self.integration, self.bob).await
    }

    async fn row(&self, id: i64) -> serde_json::Value {
        self.t
            .json_rows(&format!(
                "SELECT state, public_id, mac IS NULL AS no_mac, key_id, claimed_at IS NULL \
                 AS unclaimed, reject_reason FROM forge.act_as_assertions WHERE id = {id}"
            ))
            .await
            .remove(0)
    }

    /// As superuser, past the guard: one statement on the row, audited.
    async fn tamper(&self, id: i64, set: &str) {
        self.t
            .exec_superuser(&format!(
                "BEGIN; \
                 INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, \
                 resource_id, request_id) VALUES ('system', 'probe.changed', 'succeeded', \
                 'act_as_assertion', {id}, 'r-tamper'); \
                 ALTER TABLE forge.act_as_assertions DISABLE TRIGGER act_as_assertions_forward; \
                 UPDATE forge.act_as_assertions SET {set} WHERE id = {id}; \
                 SET CONSTRAINTS ALL IMMEDIATE; \
                 ALTER TABLE forge.act_as_assertions ENABLE ALWAYS TRIGGER act_as_assertions_forward; \
                 COMMIT;"
            ))
            .await;
    }
}

fn reason(e: &ForgeError) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

#[tokio::test]
async fn issue_writes_a_pending_row_without_secret_public_id_or_mac() {
    let w = world().await;
    let id = w.issue_for(STATE, LIFETIME).await;
    let row = w.row(id).await;
    assert_eq!(row["state"], "pending");
    assert!(row["public_id"].is_null(), "{row}");
    assert_eq!(row["no_mac"], true);
    assert!(row["key_id"].is_null());
    assert_eq!(row["unclaimed"], true);
    let audit =
        w.t.json_rows(&format!(
            "SELECT action, actor_kind, actor_user_id, project_id, details FROM forge.audit_log \
             WHERE resource_type = 'act_as_assertion' AND resource_id = {id}"
        ))
        .await;
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0]["action"], "act_as_assertion.issued");
    assert_eq!(audit[0]["actor_user_id"], w.bob.get());
    assert!(audit[0]["details"].get("public_id").is_none());
    // The state itself is never stored: its SHA-256 only.
    let stored =
        w.t.json_rows(&format!(
            "SELECT encode(state_hash, 'hex') AS h FROM forge.act_as_assertions WHERE id = {id}"
        ))
        .await;
    assert_ne!(stored[0]["h"], STATE);
    // A malformed state is refused before anything is written.
    let target = w.main();
    let err = issue(
        &w.t.db,
        &as_user(w.bob, "req-issue-bad"),
        &NewAssertion {
            integration: w.integration,
            user: w.bob,
            intent: Intent::Approve,
            project: w.project,
            subject: NewSubject::MergeRequest {
                merge_request: w.mr.id,
                head_sha: oid('a').as_str(),
                target_ref: &target,
                target_sha: oid('b').as_str(),
            },
            state: "short",
            lifetime: LIFETIME,
        },
    )
    .await
    .expect_err("short state");
    assert_eq!(err.code(), ErrorCode::RequestInvalid);
    // Another head than the merge request's: stale.
    let err = issue(
        &w.t.db,
        &as_user(w.bob, "req-issue-stale"),
        &NewAssertion {
            integration: w.integration,
            user: w.bob,
            intent: Intent::Approve,
            project: w.project,
            subject: NewSubject::MergeRequest {
                merge_request: w.mr.id,
                head_sha: oid('c').as_str(),
                target_ref: &target,
                target_sha: oid('b').as_str(),
            },
            state: STATE,
            lifetime: LIFETIME,
        },
    )
    .await
    .expect_err("stale head");
    assert_eq!(err.code(), ErrorCode::HeadChanged);
    w.t.drop_now().await;
}

#[tokio::test]
async fn claim_creates_the_secret_and_returns_it_exactly_once() {
    let w = world().await;
    let id = w.issue_for(STATE, LIFETIME).await;
    let claimed = w.claim_it(STATE, "req-claim").await.expect("claim");
    assert_eq!(claimed.user, w.bob);
    assert_eq!(claimed.employee_number.as_deref(), Some("E-2001"));
    assert_eq!(claimed.intent, Intent::Approve);
    assert_eq!(claimed.project, w.project);
    assert_eq!(claimed.iid, Some(w.mr.iid));
    assert_eq!(
        claimed.subject,
        AssertedSubject::MergeRequest {
            merge_request: w.mr.id,
            head_sha: oid('a').as_str().to_owned(),
            target_ref: w.main(),
            target_sha: oid('b').as_str().to_owned(),
        }
    );
    let row = w.row(id).await;
    assert_eq!(row["state"], "claimed");
    assert_eq!(row["public_id"], claimed.token.public_id());
    assert_eq!(row["no_mac"], false);
    assert_eq!(row["key_id"], 1);
    let verified = w.verify(&claimed.token).await.expect("verifies");
    assert_eq!(verified.id, id);
    assert_eq!(verified.subject, claimed.subject);
    let audit =
        w.t.json_rows(&format!(
            "SELECT action, actor_kind FROM forge.audit_log \
             WHERE resource_type = 'act_as_assertion' AND resource_id = {id} ORDER BY id"
        ))
        .await;
    assert_eq!(audit[1]["action"], "act_as_assertion.claimed");
    assert_eq!(audit[1]["actor_kind"], "service_account");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_second_claim_is_refused_and_returns_no_secret() {
    let w = world().await;
    w.issue_for(STATE, LIFETIME).await;
    w.claim_it(STATE, "req-claim-1").await.expect("first");
    let err = w.claim_it(STATE, "req-claim-2").await.expect_err("second");
    assert_eq!(err.code(), ErrorCode::ActAsAssertionInvalid);
    assert_eq!(reason(&err), "claim_repeated");
    // An unknown state, and another integration's claim of this one.
    let err = w
        .claim_it("s-unknownunknownunknownunknown", "req-claim-3")
        .await
        .expect_err("unknown");
    assert_eq!(reason(&err), "claim_unknown");
    let err = claim(
        &w.t.db,
        &as_sa(w.sa, "req-claim-4"),
        &w.keys,
        IntegrationId::new(w.integration.get() + 100),
        STATE,
    )
    .await
    .expect_err("other integration");
    assert_eq!(reason(&err), "claim_unknown");
    w.t.drop_now().await;
}

#[tokio::test]
async fn claim_after_expiry_is_refused() {
    let w = world().await;
    let id = w.issue_for(STATE, Duration::from_millis(300)).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let err = w.claim_it(STATE, "req-claim").await.expect_err("expired");
    assert_eq!(err.code(), ErrorCode::ActAsAssertionInvalid);
    assert_eq!(reason(&err), "claim_expired");
    assert_eq!(w.row(id).await["state"], "pending", "the row stays pending");
    let swept = sweep_expired(&w.t.db, &system("req-sweep"), SWEEP_BATCH)
        .await
        .expect("sweep");
    assert_eq!(swept, 1);
    let row = w.row(id).await;
    assert_eq!(row["state"], "rejected");
    assert_eq!(row["reject_reason"], "expired");
    assert_eq!(
        sweep_expired(&w.t.db, &system("req-sweep-2"), SWEEP_BATCH)
            .await
            .expect("sweep again"),
        0
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn assertion_expires_after_five_minutes() {
    let w = world().await;
    w.issue_for(STATE, Duration::from_millis(800)).await;
    let claimed = w.claim_it(STATE, "req-claim").await.expect("claim");
    w.verify(&claimed.token).await.expect("valid now");
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let err = w.verify(&claimed.token).await.expect_err("expired");
    assert_eq!(reason(&err), "expired");
    // The lifetime is capped at five minutes by the command and the CHECK.
    let err =
        w.t.json_rows(
            "SELECT count(*) AS n FROM forge.act_as_assertions \
             WHERE expires_at > issued_at + interval '5 minutes'",
        )
        .await;
    assert_eq!(err[0]["n"], 0);
    w.t.drop_now().await;
}

#[tokio::test]
async fn no_assertion_secret_or_unkeyed_derivative_is_stored() {
    let w = world().await;
    w.issue_for(STATE, LIFETIME).await;
    let claimed = w.claim_it(STATE, "req-claim").await.expect("claim");
    let text = claimed.token.reveal();
    let secret_hex = &text[21..];
    use sha2::Digest as _;
    let digest: [u8; 32] = sha2::Sha256::digest(claimed.token.secret()).into();
    let digest_hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    for table in ["act_as_assertions", "audit_log", "events", "jobs"] {
        let rows =
            w.t.json_rows(&format!(
                "SELECT row_to_json(t)::text AS j FROM forge.{table} t"
            ))
            .await;
        for row in rows {
            let j = row["j"].as_str().unwrap_or_default().to_owned();
            assert!(!j.contains(secret_hex), "{table} holds the secret");
            assert!(!j.contains(&digest_hex), "{table} holds its SHA-256");
            assert!(!j.contains("fga_"), "{table} holds an assertion");
        }
    }
    // bytea columns print as hex in row_to_json (\x…): the MAC is not the
    // secret's digest either.
    let mac =
        w.t.json_rows("SELECT encode(mac, 'hex') AS m FROM forge.act_as_assertions")
            .await;
    assert_ne!(mac[0]["m"], digest_hex);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_row_written_without_the_key_never_verifies() {
    let w = world().await;
    let id = w.issue_for(STATE, LIFETIME).await;
    // Someone with the database's credentials makes the row look claimed,
    // with a public id and a secret of their choosing.
    let chosen = AssertionToken::parse(&format!("fga_00000000000000ff_{}", "ab".repeat(32)))
        .expect("chosen");
    w.tamper(
        id,
        &format!(
            "state = 'claimed', public_id = '00000000000000ff', mac = '\\x{}', key_id = 1, \
             claimed_at = now()",
            "cd".repeat(32)
        ),
    )
    .await;
    let err = w.verify(&chosen).await.expect_err("never verifies");
    assert_eq!(err.code(), ErrorCode::ActAsAssertionInvalid);
    assert_eq!(reason(&err), "mac_mismatch");
    w.t.drop_now().await;
}

#[tokio::test]
async fn tampered_assertion_row_fails_the_mac() {
    let w = world().await;
    let id = w.issue_for(STATE, LIFETIME).await;
    let claimed = w.claim_it(STATE, "req-claim").await.expect("claim");
    w.tamper(id, &format!("head_sha = '{}'", oid('c').as_str()))
        .await;
    assert_eq!(
        reason(&w.verify(&claimed.token).await.expect_err("head")),
        "mac_mismatch"
    );
    w.tamper(
        id,
        &format!(
            "head_sha = '{}', target_ref = 'refs/heads/release'",
            oid('a').as_str()
        ),
    )
    .await;
    assert_eq!(
        reason(&w.verify(&claimed.token).await.expect_err("target ref")),
        "mac_mismatch"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn verification_names_the_reason_for_each_mismatch() {
    let w = world().await;
    w.issue_for(STATE, LIFETIME).await;
    let claimed = w.claim_it(STATE, "req-claim").await.expect("claim");
    let unknown = AssertionToken::generate().expect("rng");
    assert_eq!(
        reason(&w.verify(&unknown).await.expect_err("unknown")),
        "unknown"
    );
    let err = verify_assertion(
        &w.t.db,
        &w.keys,
        &claimed.token,
        IntegrationId::new(w.integration.get() + 1),
        w.bob,
    )
    .await
    .expect_err("integration");
    assert_eq!(reason(&err), "wrong_integration");
    let err = verify_assertion(&w.t.db, &w.keys, &claimed.token, w.integration, w.sa)
        .await
        .expect_err("user");
    assert_eq!(reason(&err), "wrong_user");
    let other_key = keys(8);
    let err = verify_assertion(&w.t.db, &other_key, &claimed.token, w.integration, w.bob)
        .await
        .expect_err("other key");
    assert_eq!(reason(&err), "mac_mismatch");
    w.t.drop_now().await;
}

#[tokio::test]
async fn claim_is_atomic_under_concurrency() {
    let w = world().await;
    w.issue_for(STATE, LIFETIME).await;
    let (a, b) = tokio::join!(
        w.claim_it(STATE, "req-claim-a"),
        w.claim_it(STATE, "req-claim-b")
    );
    let won = [&a, &b].iter().filter(|r| r.is_ok()).count();
    assert_eq!(won, 1, "{a:?} {b:?}");
    let lost = if a.is_err() { a } else { b };
    assert_eq!(reason(&lost.expect_err("one lost")), "claim_repeated");
    w.t.drop_now().await;
}

#[tokio::test]
async fn assertion_is_single_use() {
    let w = world().await;
    let id = w.issue_for(STATE, LIFETIME).await;
    let claimed = w.claim_it(STATE, "req-claim").await.expect("claim");
    let using = ActAsUse {
        assertion: id,
        public_id: claimed.token.public_id().to_owned(),
        integration: w.integration,
        via: w.sa,
    };
    let consume = |request: &'static str| {
        let using = using.clone();
        let db = w.t.db.clone();
        async move {
            let ctx = system(request);
            let mut tx = ChangeTx::begin(&db, &ctx).await?;
            let outcome = async {
                let entry = consume_on(&mut tx, &using, request).await?;
                Ok(Settle::Commit(
                    ChangeRecord {
                        audit: entry,
                        affected: Vec::new(),
                        events: Vec::new(),
                    },
                    (),
                ))
            }
            .await;
            tx.settle("act_as_assertion.consumed", outcome).await
        }
    };
    consume("req-use-1").await.expect("first use");
    assert_eq!(w.row(id).await["state"], "consumed");
    let err = consume("req-use-2").await.expect_err("second use");
    assert_eq!(reason(&err), "consumed");
    assert_eq!(
        reason(&w.verify(&claimed.token).await.expect_err("used")),
        "consumed"
    );
    // A consumed one is never rejected afterwards.
    assert!(!reject(&w.t.db, &system("req-reject"), id, "action_failed")
        .await
        .expect("reject"));
    w.t.drop_now().await;
}

#[tokio::test]
async fn concurrent_use_consumes_once() {
    let w = world().await;
    let id = w.issue_for(STATE, LIFETIME).await;
    let claimed = w.claim_it(STATE, "req-claim").await.expect("claim");
    let using = ActAsUse {
        assertion: id,
        public_id: claimed.token.public_id().to_owned(),
        integration: w.integration,
        via: w.sa,
    };
    let run = |request: &'static str| {
        let using = using.clone();
        let db = w.t.db.clone();
        async move {
            let ctx = system(request);
            let mut tx = ChangeTx::begin(&db, &ctx).await?;
            let outcome = async {
                let entry = consume_on(&mut tx, &using, request).await?;
                Ok(Settle::Commit(
                    ChangeRecord {
                        audit: entry,
                        affected: Vec::new(),
                        events: Vec::new(),
                    },
                    (),
                ))
            }
            .await;
            tx.settle("act_as_assertion.consumed", outcome).await
        }
    };
    let (a, b) = tokio::join!(run("req-use-a"), run("req-use-b"));
    assert_eq!(
        [&a, &b].iter().filter(|r| r.is_ok()).count(),
        1,
        "{a:?} {b:?}"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_verified_assertion_that_was_not_used_is_rejected_once() {
    let w = world().await;
    let id = w.issue_for(STATE, LIFETIME).await;
    let claimed = w.claim_it(STATE, "req-claim").await.expect("claim");
    assert!(reject(&w.t.db, &system("req-reject"), id, "action_failed")
        .await
        .expect("reject"));
    let row = w.row(id).await;
    assert_eq!(row["state"], "rejected");
    assert_eq!(row["reject_reason"], "action_failed");
    assert!(
        !reject(&w.t.db, &system("req-reject-2"), id, "action_failed")
            .await
            .expect("again")
    );
    assert_eq!(
        reason(&w.verify(&claimed.token).await.expect_err("rejected")),
        "consumed"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_guard_keeps_the_state_machine_and_the_binding() {
    let w = world().await;
    let id = w.issue_for(STATE, LIFETIME).await;
    let mut su = w.t.superuser().await;
    for set in [
        "state = 'consumed'",
        "head_sha = '3333333333333333333333333333333333333333'",
        "expires_at = expires_at - interval '1 second'",
    ] {
        let err = sqlx::raw_sql(&format!(
            "BEGIN; UPDATE forge.act_as_assertions SET {set} WHERE id = {id}; ROLLBACK;"
        ))
        .execute(&mut su)
        .await
        .expect_err("guarded");
        assert!(err.to_string().contains("act_as_assertions"), "{err}");
        sqlx::raw_sql("ROLLBACK").execute(&mut su).await.ok();
    }
    // An assertion is a person's: never a service account's.
    let err = sqlx::raw_sql(&format!(
        "BEGIN; INSERT INTO forge.act_as_assertions (integration_id, user_id, intent, \
         project_id, merge_request_id, head_sha, target_ref, target_sha, state_hash, \
         expires_at, issued_request_id) VALUES ({}, {}, 'approve', {}, {}, '{}', \
         'refs/heads/main', '{}', '\\x{}', now() + interval '1 minute', 'r'); ROLLBACK;",
        w.integration.get(),
        w.sa.get(),
        w.project.get(),
        w.mr.id.get(),
        oid('a').as_str(),
        oid('b').as_str(),
        "00".repeat(32)
    ))
    .execute(&mut su)
    .await
    .expect_err("not a person");
    assert!(
        err.to_string().contains("act_as_assertions_user_human")
            || err.to_string().contains("person"),
        "{err}"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn disabled_users_employee_number_is_kept() {
    let w = world().await;
    let disabled_now = users::disable(&w.t.db, &system("req-disable"), w.bob)
        .await
        .expect("disable");
    assert!(matches!(disabled_now, users::DisableOutcome::Disabled));
    let mut su = w.t.superuser().await;
    for set in ["employee_number = NULL", "employee_number = 'E-9999'"] {
        let err = sqlx::raw_sql(&format!(
            "UPDATE forge.users SET {set} WHERE id = {}",
            w.bob.get()
        ))
        .execute(&mut su)
        .await
        .expect_err("kept");
        let db = err.as_database_error().expect("database error");
        assert_eq!(db.code().as_deref(), Some("23514"), "{err}");
        assert_eq!(
            db.constraint(),
            Some("users_employee_number_kept_when_disabled"),
            "{err}"
        );
    }
    // Disabling and clearing in one statement is refused too.
    let mut carol = NewUser::human("carol", "carol", "carol@example.test");
    carol.employee_number = Some("E-3001".to_owned());
    let carol = users::create(&w.t.db, &system("req-carol"), &carol)
        .await
        .expect("carol");
    let err = sqlx::raw_sql(&format!(
        "UPDATE forge.users SET state = 'disabled', employee_number = NULL WHERE id = {}",
        carol.user_id.get()
    ))
    .execute(&mut su)
    .await
    .expect_err("kept on disable");
    assert!(err.to_string().contains("employee number"), "{err}");
    // The number still names exactly the disabled user: never another.
    let err = resolve_target(&w.t.db, &ActAsTarget::Employee("E-2001".into()))
        .await
        .expect_err("disabled");
    assert_eq!(err.code(), ErrorCode::ActAsTargetInvalid);
    assert_eq!(reason(&err), "disabled");
    w.t.drop_now().await;
}

#[tokio::test]
async fn act_as_unknown_and_disabled_look_the_same() {
    let w = world().await;
    let unknown = resolve_target(&w.t.db, &ActAsTarget::User(UserId::new(999_999)))
        .await
        .expect_err("unknown");
    let never = resolve_target(&w.t.db, &ActAsTarget::Employee("E-0000".into()))
        .await
        .expect_err("never signed in");
    let disabled_now = users::disable(&w.t.db, &system("req-disable"), w.bob)
        .await
        .expect("disable");
    assert!(matches!(disabled_now, users::DisableOutcome::Disabled));
    let disabled = resolve_target(&w.t.db, &ActAsTarget::User(w.bob))
        .await
        .expect_err("disabled");
    for e in [&unknown, &never, &disabled] {
        assert_eq!(e.code(), ErrorCode::ActAsTargetInvalid);
        assert_eq!(e.code().http_status(), 403);
    }
    assert_eq!(reason(&unknown), "unknown");
    assert_eq!(reason(&never), "unknown");
    assert_eq!(reason(&disabled), "disabled");
    w.t.drop_now().await;
}

#[tokio::test]
async fn act_as_service_account_or_bot_target_is_refused() {
    let w = world().await;
    let err = resolve_target(&w.t.db, &ActAsTarget::User(w.sa))
        .await
        .expect_err("itself");
    assert_eq!(reason(&err), "not_human");
    let bot = users::create(
        &w.t.db,
        &system("req-bot"),
        &NewUser {
            kind: forge_core::UserKind::Bot,
            username: "robot".to_owned(),
            display_name: "robot".to_owned(),
            email: None,
            employee_number: None,
            external_id: None,
            auth_source: users::AuthSource::Local,
            is_admin: false,
        },
    )
    .await
    .expect("bot");
    let err = resolve_target(&w.t.db, &ActAsTarget::User(bot.user_id))
        .await
        .expect_err("bot");
    assert_eq!(reason(&err), "not_human");
    // The person resolves, by id and by employee number.
    assert_eq!(
        resolve_target(&w.t.db, &ActAsTarget::User(w.bob))
            .await
            .expect("bob"),
        w.bob
    );
    assert_eq!(
        resolve_target(&w.t.db, &ActAsTarget::Employee("E-2001".into()))
            .await
            .expect("bob by number"),
        w.bob
    );
    // Exactly: case and surrounding characters matter.
    assert_eq!(
        reason(
            &resolve_target(&w.t.db, &ActAsTarget::Employee("e-2001".into()))
                .await
                .expect_err("case")
        ),
        "unknown"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn act_as_ambiguous_employee_number_fails_closed() {
    let w = world().await;
    // The unique index is gone (dropped by mistake), and a second row
    // carries the same number.
    w.t.exec_superuser("DROP INDEX forge.users_employee_number")
        .await;
    let mut dave = NewUser::human("dave", "dave", "dave@example.test");
    dave.employee_number = Some("E-2001".to_owned());
    users::create(&w.t.db, &system("req-dave"), &dave)
        .await
        .expect("dave with a duplicate number");
    let err = resolve_target(&w.t.db, &ActAsTarget::Employee("E-2001".into()))
        .await
        .expect_err("ambiguous");
    assert_eq!(err.code(), ErrorCode::ActAsTargetInvalid);
    assert_eq!(reason(&err), "ambiguous");
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_via_facts_name_the_binding_and_the_service_accounts_role() {
    let w = world().await;
    let facts = via_facts(&w.t.db, w.integration, w.sa, w.project)
        .await
        .expect("facts");
    assert_eq!(
        facts,
        ViaFacts {
            role: Some(Role::Reporter),
            bound: true
        }
    );
    let other = via_facts(
        &w.t.db,
        IntegrationId::new(w.integration.get() + 1),
        w.sa,
        w.project,
    )
    .await
    .expect("facts");
    assert!(!other.bound);
    let of = integration_of(&w.t.db, w.sa).await.expect("of");
    assert_eq!(
        of,
        Some(IntegrationOf {
            integration: w.integration,
            enabled: true
        })
    );
    assert_eq!(integration_of(&w.t.db, w.bob).await.expect("none"), None);
    w.t.drop_now().await;
}

impl World {
    /// The change context of the service account acting for bob with the
    /// claimed assertion `public_id`.
    fn as_bob_via_kanflow(&self, public_id: &str, request: &str) -> ChangeContext {
        let mut ctx = ChangeContext::new(
            Actor::UserViaServiceAccount {
                user: self.bob,
                via: self.sa,
                credential_id: 77,
                act_as_claimed: "c".repeat(64),
                act_as_key_id: 1,
                via_assertion_id: Some(public_id.to_owned()),
            },
            RequestId::parse(request).expect("request id"),
        );
        ctx.credential_id = Some(77);
        ctx
    }

    /// Bob approves through kanflow with a fresh, claimed assertion.
    async fn approve_via_kanflow(&self, state: &str, request: &str) -> i64 {
        let id = self.issue_for(state, LIFETIME).await;
        let claimed = self
            .claim_it(state, &format!("{request}-claim"))
            .await
            .expect("claim");
        let using = ActAsUse {
            assertion: id,
            public_id: claimed.token.public_id().to_owned(),
            integration: self.integration,
            via: self.sa,
        };
        let (outcome, _, ()) = review::approve_mr_decided(
            &self.t.db,
            &self.as_bob_via_kanflow(claimed.token.public_id(), request),
            (self.project, self.mr.iid),
            self.bob,
            Some(&using),
            |facts| {
                let via = facts.via.expect("act-as facts under the lock");
                assert!(via.bound);
                assert_eq!(via.role, Some(Role::Reporter));
                Ok(())
            },
            |_, _, _| (true, None),
        )
        .await
        .expect("approve via kanflow");
        assert!(
            matches!(outcome, review::ApproveOutcome::Approved { .. }),
            "{outcome:?}"
        );
        id
    }
}

/// ADR-0025 §4: an approval given acting for a person is never carried to
/// a new head, whatever the project keeps on push; a session's is.
#[tokio::test]
async fn act_as_approval_is_never_carried_to_a_new_head() {
    let w = world().await;
    let keep = forge_core::MergeSettingsValues {
        reset_approvals_on_push: false,
        ..forge_core::MergeSettingsValues::default()
    };
    let alice = w.mr.author;
    let (changed, _, ()) = review::put_merge_settings_mr_decided(
        &w.t.db,
        &as_user(alice, "req-keep"),
        w.project,
        alice,
        &keep,
        |_| Ok(()),
    )
    .await
    .expect("settings");
    assert_eq!(changed, review::SettingsOutcome::Changed);
    let id = w.approve_via_kanflow(STATE, "req-approve-bob").await;
    assert_eq!(w.row(id).await["state"], "consumed");
    // Alice's own (a session's) approval, for the comparison.
    let (outcome, _, ()) = review::approve_mr_decided(
        &w.t.db,
        &as_user(alice, "req-approve-alice"),
        (w.project, w.mr.iid),
        alice,
        None,
        |_| Ok(()),
        |_, _, _| (true, None),
    )
    .await
    .expect("alice approves");
    assert!(matches!(outcome, review::ApproveOutcome::Approved { .. }));
    let origins =
        w.t.json_rows(
            "SELECT approver_id, credential_origin FROM forge.merge_request_approvals \
             WHERE revoked_at IS NULL ORDER BY approver_id",
        )
        .await;
    assert_eq!(origins.len(), 2, "{origins:?}");
    review::record_mr_version_carrying(
        &w.t.db,
        &system("req-v2"),
        w.mr.id,
        &oid('d'),
        None,
        Some(&review::CarryPlan {
            from: oid('a'),
            approved_heads: vec![oid('a')],
        }),
    )
    .await
    .expect("v2");
    let live =
        w.t.json_rows(
            "SELECT approver_id, head_sha, credential_origin FROM forge.merge_request_approvals \
             WHERE revoked_at IS NULL",
        )
        .await;
    assert_eq!(
        live.len(),
        1,
        "only the session's approval is carried: {live:?}"
    );
    assert_eq!(live[0]["approver_id"], alice.get());
    assert_eq!(live[0]["head_sha"], oid('d').as_str());
    assert_eq!(live[0]["credential_origin"], "session");
    let ended =
        w.t.json_rows(&format!(
            "SELECT revoked_reason FROM forge.merge_request_approvals WHERE approver_id = {}",
            w.bob.get()
        ))
        .await;
    assert_eq!(ended[0]["revoked_reason"], "new_commits");
    w.t.drop_now().await;
}

/// The act-as approve records both people in its audit rows and consumes
/// the assertion in the same transaction; a second use rolls the whole
/// change back.
#[tokio::test]
async fn an_act_as_approval_audits_both_people_and_consumes_once() {
    let w = world().await;
    let id = w.approve_via_kanflow(STATE, "req-approve").await;
    let audit =
        w.t.json_rows(
            "SELECT action, actor_kind, actor_user_id, actor_via_id, credential_id, \
             via_assertion_id FROM forge.audit_log WHERE request_id = 'req-approve' ORDER BY id",
        )
        .await;
    let actions: Vec<&str> = audit.iter().filter_map(|a| a["action"].as_str()).collect();
    assert_eq!(
        actions,
        vec!["merge_request.approved", "act_as_assertion.consumed"],
        "{audit:?}"
    );
    for a in &audit {
        assert_eq!(a["actor_kind"], "user_via_service_account");
        assert_eq!(a["actor_user_id"], w.bob.get());
        assert_eq!(a["actor_via_id"], w.sa.get());
        assert_eq!(a["credential_id"], 77);
    }
    let public_id = audit[0]["via_assertion_id"]
        .as_str()
        .expect("public id")
        .to_owned();
    let event =
        w.t.json_rows(
            "SELECT actor_kind, actor_via_id FROM forge.events \
             WHERE request_id = 'req-approve'",
        )
        .await;
    assert_eq!(event[0]["actor_kind"], "user_via_service_account");
    // Using the consumed assertion again: refused, nothing written.
    let again = review::unapprove_mr_decided(
        &w.t.db,
        &w.as_bob_via_kanflow(&public_id, "req-again"),
        (w.project, w.mr.iid),
        w.bob,
        Some(&ActAsUse {
            assertion: id,
            public_id: public_id.clone(),
            integration: w.integration,
            via: w.sa,
        }),
        |_| Ok(()),
    )
    .await
    .expect_err("consumed");
    assert_eq!(reason(&again), "consumed");
    let live =
        w.t.json_rows(
            "SELECT count(*) AS n FROM forge.merge_request_approvals WHERE revoked_at IS NULL",
        )
        .await;
    assert_eq!(live[0]["n"], 1, "the unapprove rolled back");
    w.t.drop_now().await;
}

/// Review (security LOW): a state names one confirmation per integration.
#[tokio::test]
async fn a_state_is_used_once_per_integration() {
    let w = world().await;
    w.issue_for(STATE, LIFETIME).await;
    let target = w.main();
    let err = issue(
        &w.t.db,
        &as_user(w.bob, "req-issue-again"),
        &NewAssertion {
            integration: w.integration,
            user: w.bob,
            intent: Intent::Approve,
            project: w.project,
            subject: NewSubject::MergeRequest {
                merge_request: w.mr.id,
                head_sha: oid('a').as_str(),
                target_ref: &target,
                target_sha: oid('b').as_str(),
            },
            state: STATE,
            lifetime: LIFETIME,
        },
    )
    .await
    .expect_err("state in use");
    assert_reason(&err, ErrorCode::RequestInvalid, "param_invalid");
    assert!(err.to_string().contains("rule=in_use"), "{err}");
    w.t.drop_now().await;
}

impl World {
    async fn approve_with(
        &self,
        id: i64,
        public_id: &str,
        request: &str,
    ) -> Result<(), ForgeError> {
        review::approve_mr_decided(
            &self.t.db,
            &self.as_bob_via_kanflow(public_id, request),
            (self.project, self.mr.iid),
            self.bob,
            Some(&ActAsUse {
                assertion: id,
                public_id: public_id.to_owned(),
                integration: self.integration,
                via: self.sa,
            }),
            |_| Ok(()),
            |_, _, _| (true, None),
        )
        .await
        .map(drop)
    }
}

/// Review (security LOW): the approve's transaction reads the integration
/// and its service account again under the lock; a disable committed after
/// the request was authenticated refuses it and writes nothing.
#[tokio::test]
async fn an_act_as_approval_rechecks_the_integration_and_the_account_under_the_lock() {
    let w = world().await;
    let id = w.issue_for(STATE, LIFETIME).await;
    let claimed = w.claim_it(STATE, "req-claim").await.expect("claim");
    let public_id = claimed.token.public_id().to_owned();
    let off = integrations::update(
        &w.t.db,
        &system("req-off"),
        w.integration,
        &integrations::Changes {
            enabled: Some(false),
            event_types: None,
        },
    )
    .await
    .expect("disable integration");
    assert!(matches!(off, integrations::UpdateOutcome::Updated(_)));
    let err = w
        .approve_with(id, &public_id, "req-approve-off")
        .await
        .expect_err("integration disabled");
    assert_reason(&err, ErrorCode::Forbidden, "integration_disabled");
    assert_eq!(w.row(id).await["state"], "claimed", "nothing consumed");
    let on = integrations::update(
        &w.t.db,
        &system("req-on"),
        w.integration,
        &integrations::Changes {
            enabled: Some(true),
            event_types: None,
        },
    )
    .await
    .expect("enable integration");
    assert!(matches!(on, integrations::UpdateOutcome::Updated(_)));
    let gone = users::disable(&w.t.db, &system("req-sa-off"), w.sa)
        .await
        .expect("disable sa");
    assert!(matches!(gone, users::DisableOutcome::Disabled));
    let err = w
        .approve_with(id, &public_id, "req-approve-sa-off")
        .await
        .expect_err("service account disabled");
    assert_reason(&err, ErrorCode::AccountDisabled, "via_disabled");
    let approvals =
        w.t.json_rows("SELECT count(*) AS n FROM forge.merge_request_approvals")
            .await;
    assert_eq!(approvals[0]["n"], 0);
    w.t.drop_now().await;
}

/// M3 X3 (ADR-0025 Amendment X3): a binding assertion seals the kanflow
/// project and the setting; a changed setting never verifies, and the
/// subject of the other shape is refused at the issue.
#[tokio::test]
async fn a_binding_assertion_round_trips_and_its_setting_is_sealed() {
    let w = world().await;
    let issue_binding = |intent, resolves, state: &'static str| {
        let w = &w;
        async move {
            issue(
                &w.t.db,
                &as_user(w.bob, "req-issue-binding"),
                &NewAssertion {
                    integration: w.integration,
                    user: w.bob,
                    intent,
                    project: w.project,
                    subject: NewSubject::Binding {
                        external_project_id: "KF-7",
                        resolves_work_items: resolves,
                    },
                    state,
                    lifetime: LIFETIME,
                },
            )
            .await
        }
    };
    let id = issue_binding(Intent::BindingSet, Some(false), STATE)
        .await
        .expect("issue");
    let claimed = w.claim_it(STATE, "req-claim-binding").await.expect("claim");
    assert_eq!(claimed.intent, Intent::BindingSet);
    assert_eq!(claimed.iid, None);
    let expected = AssertedSubject::Binding {
        external_project_id: "KF-7".to_owned(),
        resolves_work_items: Some(false),
    };
    assert_eq!(claimed.subject, expected);
    let verified = w.verify(&claimed.token).await.expect("verifies");
    assert_eq!(verified.subject, expected);
    // The setting is in the MAC: a row changed to resolve never verifies.
    w.tamper(id, "resolves_work_items = true").await;
    let err = w.verify(&claimed.token).await.expect_err("tampered");
    assert_reason(&err, ErrorCode::ActAsAssertionInvalid, "mac_mismatch");
    // A removal carries no setting; a set without one is a caller bug.
    issue_binding(
        Intent::BindingRemove,
        None,
        "s-remove-0123456789abcdefghijklmn",
    )
    .await
    .expect("remove issued");
    let err = issue_binding(
        Intent::BindingSet,
        None,
        "s-broken-0123456789abcdefghijklmn",
    )
    .await
    .expect_err("set without a setting");
    assert_reason(&err, ErrorCode::Internal, "assertion_subject_mismatch");
    let err = issue_binding(Intent::Approve, None, "s-approve-0123456789abcdefghijklm")
        .await
        .expect_err("approve on a binding");
    assert_reason(&err, ErrorCode::Internal, "assertion_subject_mismatch");
    w.t.drop_now().await;
}
