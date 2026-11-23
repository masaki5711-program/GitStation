//! Document revisions and edit claims against a migrated database (plan 13
//! stage 4 part A; migration 20270105100000): the earliest open revision
//! holds a document, a version keeps `since`, a closed merge request stops
//! counting; a claim is taken, extended, refused to another person while
//! live, replaced once lapsed, released by its holder and revoked with an
//! audit row.

use forge_core::doc_revision::DocKey;
use forge_core::ids::{ProjectId, UserId};
use forge_core::{RefName, RequestId};
use forge_errors::ErrorCode;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, CreatedProject, NewProject};
use crate::review::{self, NewMergeRequest, StoredMergeRequest};
use crate::test_support::*;
use crate::users::{self, NewUser};

const HEAD: &str = "1111111111111111111111111111111111111111";
const HEAD2: &str = "2222222222222222222222222222222222222222";

fn ctx(actor: Actor, request: &str) -> ChangeContext {
    ChangeContext::new(actor, RequestId::parse(request).expect("request id"))
}

struct World {
    t: FreshDb,
    alice: UserId,
    bob: UserId,
    app: CreatedProject,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &ctx(Actor::System, "req-user-alice"),
        &NewUser::human("alice", "Alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let bob = users::create(
        &t.db,
        &ctx(Actor::System, "req-user-bob"),
        &NewUser::human("bob", "Bob", "bob@example.test"),
    )
    .await
    .expect("bob");
    let app = projects::create(
        &t.db,
        &ctx(Actor::System, "req-project-app"),
        &NewProject::new("app", alice.namespace_id, "App", alice.user_id),
    )
    .await
    .expect("app");
    World {
        t,
        alice: alice.user_id,
        bob: bob.user_id,
        app,
    }
}

impl World {
    fn project(&self) -> ProjectId {
        self.app.project_id
    }

    async fn open_mr(&self, source: &str) -> StoredMergeRequest {
        review::open_mr_decided(
            &self.t.db,
            &ctx(Actor::User(self.alice), &format!("req-open-{source}")),
            self.project(),
            self.alice,
            &NewMergeRequest {
                title: format!("revise {source}"),
                description: String::new(),
                source_branch: RefName::parse(&format!("refs/heads/{source}")).expect("branch"),
                target_branch: RefName::parse("refs/heads/main").expect("branch"),
                draft: false,
            },
            |_| Ok(()),
        )
        .await
        .map(|(stored, ())| stored)
        .expect("open")
    }

    async fn take(
        &self,
        who: UserId,
        doc: &str,
        request: &str,
    ) -> Result<(ClaimOutcome, EditClaim), ForgeError> {
        take_edit_claim_decided(
            &self.t.db,
            &ctx(Actor::User(who), request),
            self.project(),
            who,
            doc,
            None,
            |_| Ok(()),
        )
        .await
        .map(|(o, c, ())| (o, c))
    }

    async fn take_for(
        &self,
        iid: i64,
        request: &str,
    ) -> Result<(ClaimOutcome, EditClaim, ()), ForgeError> {
        take_edit_claim_decided(
            &self.t.db,
            &ctx(Actor::User(self.alice), request),
            self.project(),
            self.alice,
            "QP-07-001",
            Some(iid),
            |_| Ok(()),
        )
        .await
    }

    async fn lapse(&self, doc: &str) {
        self.t
            .exec_superuser(&format!(
                "UPDATE forge.document_edit_claims SET claimed_at = now() - interval '2 hours', \
                 expires_at = now() - interval '1 hour' WHERE doc_id = '{doc}'"
            ))
            .await;
    }

    async fn audit(&self, action: &str) -> Vec<serde_json::Value> {
        sqlx::query_scalar(
            "SELECT details FROM forge.audit_log WHERE action = $1 AND resource_type = 'project' \
             AND resource_id = $2 ORDER BY id",
        )
        .bind(action)
        .bind(self.project().get())
        .fetch_all(&mut self.t.superuser().await)
        .await
        .expect("audit")
    }
}

fn en(id: &str) -> DocKey {
    DocKey {
        doc_id: id.into(),
        lang: Some("en".into()),
    }
}

#[tokio::test]
async fn the_earliest_open_merge_request_holds_a_document() {
    let w = world().await;
    let first = w.open_mr("first").await;
    let second = w.open_mr("second").await;
    let qp = DocKey::original("QP-07-001");
    record_merge_request_documents(
        &w.t.db,
        w.project(),
        first.id,
        HEAD,
        &[qp.clone(), en("QP-07-001")],
    )
    .await
    .expect("first");
    record_merge_request_documents(
        &w.t.db,
        w.project(),
        second.id,
        HEAD,
        std::slice::from_ref(&qp),
    )
    .await
    .expect("second");

    let open = open_revisions(&w.t.db, w.project(), std::slice::from_ref(&qp))
        .await
        .expect("open");
    let iids: Vec<i64> = open.iter().map(|r| r.iid).collect();
    assert_eq!(iids, vec![first.iid, second.iid]);
    assert_eq!(open[0].author, w.alice);
    assert_eq!(open[0].head_sha, HEAD);
    // The translation is a document of its own.
    let t = open_revisions(&w.t.db, w.project(), &[en("QP-07-001")])
        .await
        .expect("en");
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].key, en("QP-07-001"));

    // A new version keeps `since` and drops what it no longer revises.
    let since = open[0].since.clone();
    record_merge_request_documents(
        &w.t.db,
        w.project(),
        first.id,
        HEAD2,
        std::slice::from_ref(&qp),
    )
    .await
    .expect("again");
    let again = open_revisions(&w.t.db, w.project(), &[qp.clone(), en("QP-07-001")])
        .await
        .expect("open");
    assert_eq!(again[0].since, since);
    assert_eq!(again[0].head_sha, HEAD2);
    assert!(again.iter().all(|r| r.key == qp), "{again:?}");

    // A closed merge request no longer counts.
    let _closed = review::close_mr_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-close-first"),
        w.project(),
        first.iid,
        w.alice,
        |_| Ok(()),
    )
    .await
    .expect("close");
    let left = open_revisions(&w.t.db, w.project(), &[qp])
        .await
        .expect("open");
    assert_eq!(
        left.iter().map(|r| r.iid).collect::<Vec<_>>(),
        vec![second.iid]
    );
    assert!(open_revisions(&w.t.db, w.project(), &[])
        .await
        .expect("none")
        .is_empty());
}

#[tokio::test]
async fn a_claim_is_taken_extended_and_refused_to_another_person_while_live() {
    let w = world().await;
    let (o, c) = w
        .take(w.alice, "QP-07-001", "req-take")
        .await
        .expect("take");
    assert_eq!(o, ClaimOutcome::Taken);
    assert_eq!(c.holder, w.alice);
    let (o2, c2) = w
        .take(w.alice, "QP-07-001", "req-extend")
        .await
        .expect("extend");
    assert_eq!(o2, ClaimOutcome::Extended);
    assert_eq!(c2.claimed_at, c.claimed_at);
    assert!(c2.expires_at >= c.expires_at);

    let e = w
        .take(w.bob, "QP-07-001", "req-bob")
        .await
        .expect_err("held");
    assert_eq!(e.code(), ErrorCode::DocEditClaimHeld);
    assert!(e
        .context()
        .iter()
        .any(|(k, v)| *k == "holder_id" && v == &w.alice.get().to_string()));
    let e = release_edit_claim_decided(
        &w.t.db,
        &ctx(Actor::User(w.bob), "req-bob-release"),
        w.project(),
        w.bob,
        "QP-07-001",
        |_| Ok(()),
    )
    .await
    .expect_err("not bob's");
    assert_eq!(e.code(), ErrorCode::DocEditClaimHeld);

    // Lapsed: nobody holds it, and the next person takes it.
    w.lapse("QP-07-001").await;
    assert_eq!(
        edit_claim(&w.t.db, w.project(), "QP-07-001")
            .await
            .expect("read"),
        None
    );
    let (o3, c3) = w.take(w.bob, "QP-07-001", "req-bob-2").await.expect("bob");
    assert_eq!(o3, ClaimOutcome::Taken);
    assert_eq!(c3.holder, w.bob);
    assert_eq!(
        edit_claim(&w.t.db, w.project(), "QP-07-001")
            .await
            .expect("read")
            .map(|c| c.holder),
        Some(w.bob)
    );
}

#[tokio::test]
async fn a_claim_names_an_open_merge_request_of_the_project_only() {
    let w = world().await;
    let mr = w.open_mr("claimed").await;
    let (_, c, ()) = w.take_for(mr.iid, "req-with-mr").await.expect("with");
    assert_eq!(c.merge_request_iid, Some(mr.iid));
    let e = w
        .take_for(mr.iid + 100, "req-no-mr")
        .await
        .expect_err("absent");
    assert_eq!(e.code(), ErrorCode::NotFound);
    // Extending without naming one keeps the link.
    let (_, c, ()) = take_edit_claim_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-keep"),
        w.project(),
        w.alice,
        "QP-07-001",
        None,
        |_| Ok(()),
    )
    .await
    .expect("keep");
    assert_eq!(c.merge_request_iid, Some(mr.iid));
}

#[tokio::test]
async fn a_claim_is_released_by_its_holder_or_on_open_and_revoked_with_an_audit_row() {
    let w = world().await;
    w.take(w.alice, "A-1", "req-a1").await.expect("a1");
    release_edit_claim_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-a1-release"),
        w.project(),
        w.alice,
        "A-1",
        |_| Ok(()),
    )
    .await
    .expect("release");
    assert_eq!(
        edit_claim(&w.t.db, w.project(), "A-1").await.expect("read"),
        None
    );
    let e = release_edit_claim_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-a1-again"),
        w.project(),
        w.alice,
        "A-1",
        |_| Ok(()),
    )
    .await
    .expect_err("none");
    assert_eq!(e.code(), ErrorCode::NotFound);

    // Opening a merge request releases the opener's claims on its documents.
    w.take(w.alice, "A-2", "req-a2").await.expect("a2");
    w.take(w.bob, "B-1", "req-b1").await.expect("b1");
    let n = release_claims_of(&w.t.db, w.project(), w.alice, &["A-2".into(), "B-1".into()])
        .await
        .expect("released");
    assert_eq!(n, 1);
    assert!(edit_claim(&w.t.db, w.project(), "B-1")
        .await
        .expect("read")
        .is_some());

    // A Maintainer revokes bob's claim: audited, the holder named.
    let (gone, ()) = revoke_edit_claim_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-revoke"),
        w.project(),
        w.alice,
        "B-1",
        |_| Ok(()),
    )
    .await
    .expect("revoke");
    assert_eq!(gone.holder, w.bob);
    let rows = w.audit("document.edit_claim.revoked").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["holder_id"], serde_json::json!(w.bob.get()));
    assert_eq!(rows[0]["doc_id"], serde_json::json!("B-1"));
    let e = revoke_edit_claim_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-revoke-again"),
        w.project(),
        w.alice,
        "B-1",
        |_| Ok(()),
    )
    .await
    .expect_err("none");
    assert_eq!(e.code(), ErrorCode::NotFound);
}

#[tokio::test]
async fn a_refused_decision_writes_nothing() {
    let w = world().await;
    let e = take_edit_claim_decided(
        &w.t.db,
        &ctx(Actor::User(w.bob), "req-refused"),
        w.project(),
        w.bob,
        "A-1",
        None,
        |_| -> Result<(), ForgeError> { Err(ForgeError::new(ErrorCode::Forbidden)) },
    )
    .await
    .expect_err("refused");
    assert_eq!(e.code(), ErrorCode::Forbidden);
    assert_eq!(
        edit_claim(&w.t.db, w.project(), "A-1").await.expect("read"),
        None
    );
}
