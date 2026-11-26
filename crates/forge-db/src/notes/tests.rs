//! Plan 20 N1 (ADR-0040 §6): the notes pointer on a migrated database.
//! Fixtures are written through the user and project commands; synthetic
//! data only.

use forge_core::ids::UserId;
use forge_core::RequestId;
use forge_errors::ErrorCode;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

async fn user(t: &FreshDb, name: &str) -> users::Created {
    users::create(
        &t.db,
        &ctx(&format!("req-user-{name}")),
        &NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("user")
}

async fn project(t: &FreshDb, segment: &str, u: &users::Created) -> ProjectId {
    let new = NewProject::new(segment, u.namespace_id, segment, u.user_id);
    projects::create(&t.db, &ctx(&format!("req-p-{segment}")), &new)
        .await
        .expect("project")
        .project_id
}

#[tokio::test]
async fn pointer_is_written_once_and_read_back_live() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    assert_eq!(
        own_namespace(&t.db, alice.user_id).await.expect("ns"),
        Some((alice.namespace_id, "alice".to_owned()))
    );
    assert_eq!(
        own_namespace(&t.db, UserId::new(999_999))
            .await
            .expect("ns"),
        None
    );
    assert_eq!(
        notes_of(&t.db, alice.user_id).await.expect("read"),
        NotesState::None
    );
    let notes = project(&t, NOTES_SEGMENT, &alice).await;
    link(&t.db, alice.user_id, notes, None).await.expect("link");
    assert_eq!(
        notes_of(&t.db, alice.user_id).await.expect("read"),
        NotesState::Live(notes)
    );
    // A second pointer without naming the stale one is refused.
    let other = project(&t, "other", &alice).await;
    let err = link(&t.db, alice.user_id, other, None)
        .await
        .expect_err("refused");
    assert_eq!(err.code(), ErrorCode::NotesCreateRefused);
    // Replacing names the project the caller saw; a wrong one is refused.
    let err = link(&t.db, alice.user_id, other, Some(other))
        .await
        .expect_err("refused");
    assert_eq!(err.code(), ErrorCode::NotesCreateRefused);
    link(&t.db, alice.user_id, other, Some(notes))
        .await
        .expect("replaced");
    // `other` is not `<alice>/notes`: the pointer is stale (moved).
    assert_eq!(
        notes_of(&t.db, alice.user_id).await.expect("read"),
        NotesState::Stale {
            project: other,
            reason: "moved"
        }
    );
}

#[tokio::test]
async fn a_project_is_the_notes_of_one_person_only() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let notes = project(&t, NOTES_SEGMENT, &alice).await;
    link(&t.db, alice.user_id, notes, None).await.expect("link");
    let err = link(&t.db, bob.user_id, notes, None)
        .await
        .expect_err("refused");
    assert_eq!(err.code(), ErrorCode::NotesCreateRefused);
    // Bob's pointer at Alice's project would be stale anyway; none exists.
    assert_eq!(
        notes_of(&t.db, bob.user_id).await.expect("read"),
        NotesState::None
    );
}
