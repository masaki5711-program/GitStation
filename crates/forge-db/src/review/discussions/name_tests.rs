//! The names read with a discussion and its notes (MR B0): a missing
//! user fails the read (FG-SYS-0001) — a note is never silently dropped
//! from its thread.

use super::*;

fn discussion_row(username: Option<&str>) -> DiscussionRow {
    DiscussionRow {
        id: 5,
        merge_request_id: 2,
        kind: "general".to_owned(),
        version_no: None,
        path: None,
        side: None,
        line: None,
        started_by: 3,
        started_by_username: username.map(str::to_owned),
        started_by_display_name: Some("Rita R.".to_owned()),
        resolved: false,
        resolved_by: None,
        resolved_at: None,
        revision: 1,
        created_at: "2026-11-03T00:00:00Z".to_owned(),
    }
}

fn note_row(id: i64, display_name: Option<&str>) -> NoteRow {
    NoteRow {
        id,
        discussion_id: 5,
        author_id: 4,
        author_username: Some("bob".to_owned()),
        author_display_name: display_name.map(str::to_owned),
        body: "b".to_owned(),
        created_at: "2026-11-03T00:00:00Z".to_owned(),
    }
}

fn context<'a>(err: &'a ForgeError, key: &str) -> Option<&'a str> {
    err.context()
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.as_str())
}

#[test]
fn a_discussion_without_its_starters_names_fails_the_read() {
    let err = discussion_row(None).parse().expect_err("nameless");
    assert_eq!(err.code(), ErrorCode::Internal, "{err}");
    assert_eq!(context(&err, "reason"), Some("stored_started_by_missing"));
    assert_eq!(context(&err, "discussion_id"), Some("5"));
    let named = discussion_row(Some("rita")).parse().expect("named");
    assert_eq!(
        named.started_by_name,
        UserName {
            username: "rita".to_owned(),
            display_name: "Rita R.".to_owned(),
        }
    );
}

#[test]
fn a_note_without_its_authors_names_fails_the_whole_thread() {
    let err = note_row(8, None).parse().expect_err("nameless");
    assert_eq!(err.code(), ErrorCode::Internal, "{err}");
    assert_eq!(context(&err, "reason"), Some("stored_note_author_missing"));
    assert_eq!(context(&err, "note_id"), Some("8"));
    // attach() fails rather than skipping the note.
    let mut threads = vec![discussion_row(Some("rita")).parse().expect("thread")];
    let err = attach(
        &mut threads,
        vec![note_row(7, Some("Bob B.")), note_row(8, None)],
    )
    .expect_err("never dropped");
    assert_eq!(context(&err, "note_id"), Some("8"));
    let mut threads = vec![discussion_row(Some("rita")).parse().expect("thread")];
    attach(&mut threads, vec![note_row(7, Some("Bob B."))]).expect("named");
    assert_eq!(threads[0].notes[0].author_name.display_name, "Bob B.");
}

/// The column lists build the names from [`user_name_columns`] (one
/// spelling for every table).
#[test]
fn the_column_lists_use_the_shared_sub_selects() {
    assert!(discussion_columns().contains(&user_name_columns(
        "merge_request_discussions",
        "started_by",
        "started_by"
    )));
    assert!(note_columns().contains(&user_name_columns(
        "merge_request_notes",
        "author_id",
        "author"
    )));
}
