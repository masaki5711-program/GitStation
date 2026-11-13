//! The author names read with a merge request (MR B0): a missing user is
//! FG-SYS-0001, never a nameless row; the constant column list cannot
//! drift from [`user_name_columns`]; and every write path's `RETURNING`
//! row carries the names, not only the reads.

use forge_core::RequestId;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn row(username: Option<&str>, display_name: Option<&str>) -> Row {
    Row {
        id: 41,
        project_id: 7,
        iid: 1,
        title: "T".to_owned(),
        description: String::new(),
        author_id: 3,
        author_username: username.map(str::to_owned),
        author_display_name: display_name.map(str::to_owned),
        source_branch: "refs/heads/feature".to_owned(),
        target_branch: "refs/heads/main".to_owned(),
        state: "open".to_owned(),
        draft: false,
        head_sha: None,
        revision: 1,
        created_at: "2026-11-03T00:00:00Z".to_owned(),
        updated_at: "2026-11-03T00:00:00Z".to_owned(),
        closed_at: None,
        closed_by: None,
        merged_by: None,
        merged_by_username: None,
        merged_by_display_name: None,
    }
}

fn context<'a>(err: &'a ForgeError, key: &str) -> Option<&'a str> {
    err.context()
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.as_str())
}

#[test]
fn a_merge_request_without_its_author_names_fails_the_read() {
    for (username, display_name) in [(None, Some("Dave")), (Some("dave"), None), (None, None)] {
        let err = row(username, display_name)
            .parse()
            .expect_err("a nameless merge request is never returned");
        assert_eq!(err.code(), ErrorCode::Internal, "{err}");
        assert_eq!(context(&err, "reason"), Some("stored_author_missing"));
        assert_eq!(context(&err, "merge_request_id"), Some("41"));
    }
    let named = row(Some("dave.k"), Some("Dave Kowalski"))
        .parse()
        .expect("named");
    assert_eq!(
        named.author_name,
        UserName {
            username: "dave.k".to_owned(),
            display_name: "Dave Kowalski".to_owned(),
        }
    );
}

#[test]
fn user_name_without_a_name_is_fg_sys_0001_with_its_reason() {
    let err =
        user_name(None, Some("x".to_owned()), "some_reason", "thing_id", 9).expect_err("missing");
    assert_eq!(err.code(), ErrorCode::Internal);
    assert_eq!(context(&err, "reason"), Some("some_reason"));
    assert_eq!(context(&err, "thing_id"), Some("9"));
}

/// `COLUMNS` spells the sub-selects out to stay a constant: it must be
/// exactly what [`user_name_columns`] builds for the author.
#[test]
fn the_constant_columns_hold_the_same_author_sub_selects() {
    let built = user_name_columns("merge_requests", "author_id", "author");
    assert!(
        COLUMNS.contains(&built),
        "COLUMNS drifted from user_name_columns:\n{COLUMNS}\n---\n{built}"
    );
}

fn as_user(user: UserId, request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::User(user),
        RequestId::parse(request).expect("request id"),
    )
}

/// The commands answer from their `RETURNING` row (open, edit, close,
/// reopen); each carries the author's names — the author's, not the
/// caller's.
#[tokio::test]
async fn every_write_returns_the_author_names() {
    let t = FreshDb::migrated().await;
    let system = ChangeContext::new(Actor::System, RequestId::parse("req-u").expect("id"));
    let dave = users::create(
        &t.db,
        &system,
        &NewUser::human("dave.k", "Dave Kowalski", "dave@example.test"),
    )
    .await
    .expect("dave");
    let olga = users::create(
        &t.db,
        &system,
        &NewUser::human("olga", "Olga O.", "olga@example.test"),
    )
    .await
    .expect("olga")
    .user_id;
    let web = projects::create(
        &t.db,
        &system,
        &NewProject::new("web", dave.namespace_id, "Web", dave.user_id),
    )
    .await
    .expect("web");
    let (p, author) = (web.project_id, dave.user_id);
    let want = UserName {
        username: "dave.k".to_owned(),
        display_name: "Dave Kowalski".to_owned(),
    };
    let branch = |b: &str| RefName::parse(&format!("refs/heads/{b}")).expect("branch");
    let new = NewMergeRequest {
        title: "T".to_owned(),
        description: String::new(),
        source_branch: branch("feature"),
        target_branch: branch("main"),
        draft: false,
    };
    let (opened, ()) =
        open_mr_decided(
            &t.db,
            &as_user(author, "req-o"),
            p,
            author,
            &new,
            |_| Ok(()),
        )
        .await
        .expect("open");
    assert_eq!(opened.author_name, want, "open");
    let patch = MergeRequestPatch {
        title: Some("U".to_owned()),
        ..MergeRequestPatch::default()
    };
    let (_, edited, ()) =
        edit_mr_decided(&t.db, &as_user(olga, "req-e"), p, 1, olga, &patch, |_| {
            Ok(())
        })
        .await
        .expect("edit");
    assert_eq!(edited.author_name, want, "edit by another");
    let (_, closed, ()) = close_mr_decided(&t.db, &as_user(olga, "req-c"), p, 1, olga, |_| Ok(()))
        .await
        .expect("close");
    assert_eq!(closed.author_name, want, "close");
    let (_, reopened, ()) =
        reopen_mr_decided(&t.db, &as_user(olga, "req-r"), p, 1, olga, None, |_| Ok(()))
            .await
            .expect("reopen");
    assert_eq!(reopened.author_name, want, "reopen");
    let read = get(&t.db, p, 1).await.expect("get").expect("there");
    assert_eq!(read.author_name, want, "get");
    t.drop_now().await;
}
