use super::*;
use crate::doctype::{parse_doctype, OwnerKind};
use crate::lint::{lint, FileReport, Source};

fn lint_note(doctype: &str, path: &str, note: &str) -> Vec<FileReport> {
    let doctypes = [Source {
        path: DOCTYPE_PATH,
        bytes: doctype.as_bytes(),
    }];
    let documents = [Source {
        path,
        bytes: note.as_bytes(),
    }];
    lint(&doctypes, &documents)
}

fn rules(reports: &[FileReport]) -> Vec<String> {
    reports
        .iter()
        .flat_map(|r| r.problems.iter().map(|p| p.rule.to_string()))
        .collect()
}

/// Plan 20 N1: `owner_kind: user` parses, and the lint refuses a note
/// whose `owner` is not one person's login (a group path, spaces) while
/// a login passes.
#[test]
fn owner_kind_user_lint() {
    let ty = parse_doctype("note", DOCTYPE_YML.as_bytes()).expect("note type");
    assert_eq!(ty.owner_kind, Some(OwnerKind::User));
    let id = "N-20261126-101500";
    let path = note_path(id);
    let ok = quick_note(id, "メモ", "alice", "本文");
    assert!(lint_note(DOCTYPE_YML, &path, &ok).is_empty());
    for owner in ["team/web", "two words", "-dash", "@alice"] {
        let bad = quick_note(id, "メモ", owner, "本文");
        let found = rules(&lint_note(DOCTYPE_YML, &path, &bad));
        assert_eq!(found, vec!["owner_not_user".to_owned()], "{owner:?}");
    }
    // Without `owner_kind` a group path is a fine owner (a wiki page).
    let plain = DOCTYPE_YML.replace("owner_kind: user\n", "");
    let group = quick_note(id, "メモ", "team/web", "本文");
    assert!(lint_note(&plain, &path, &group).is_empty());
    // Any other kind is a broken definition.
    let wrong = DOCTYPE_YML.replace("owner_kind: user", "owner_kind: group");
    assert!(parse_doctype("note", wrong.as_bytes()).is_err());
}

/// The first commit of a notes project passes the lint as it is.
#[test]
fn seed_passes_doc_lint() {
    let files = seed();
    let doctypes = [Source {
        path: files[0].0,
        bytes: files[0].1.as_bytes(),
    }];
    let documents = [Source {
        path: files[1].0,
        bytes: files[1].1.as_bytes(),
    }];
    assert!(lint(&doctypes, &documents).is_empty());
}

#[test]
fn note_id_is_utc_and_bounded() {
    assert_eq!(note_id(0).as_deref(), Some("N-19700101-000000"));
    // 2026-11-26T01:15:00Z
    assert_eq!(note_id(1_795_655_700).as_deref(), Some("N-20261126-011500"));
    // 2000-02-29T23:59:59Z (a leap day)
    assert_eq!(note_id(951_868_799).as_deref(), Some("N-20000229-235959"));
    assert_eq!(note_id(-62_000_000_000), None);
}

#[test]
fn title_cannot_change_the_front_matter() {
    let id = "N-20261126-101500";
    for title in [
        "a: b",
        "\"q\" \\",
        "line\nowner: bob",
        "{ x: y }",
        "nel\u{85}x",
        "bom\u{feff}x",
        "c1\u{9b}x",
    ] {
        let note = quick_note(id, title, "alice", "body");
        assert!(
            lint_note(DOCTYPE_YML, &note_path(id), &note).is_empty(),
            "{title:?}"
        );
    }
}

#[test]
fn default_title_is_the_first_line() {
    assert_eq!(default_title("\n# 打ち合わせ\nbody", "N-1"), "打ち合わせ");
    assert_eq!(default_title("  \n", "N-1"), "N-1");
}
