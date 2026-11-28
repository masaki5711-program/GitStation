//! The pure part of the document revisions (plan 13 stage 4 part A): which
//! `.md` files changed, which of them are documents, and the refusal.

use super::*;

fn blob(path: &str, oid: char) -> ListedBlob {
    ListedBlob {
        path: path.to_owned(),
        oid: oid.to_string().repeat(40),
        size: 10,
    }
}

fn listing(blobs: Vec<ListedBlob>) -> BlobListing {
    BlobListing {
        blobs,
        truncated: false,
    }
}

const DOC: &str = "---\ndoctype: qms.procedure\nid: QP-07-001\ntitle: T\nowner: qa\n---\n\n# T\n";

#[test]
fn a_document_is_named_after_its_id_and_a_translation_after_its_language() {
    assert_eq!(
        document_key("docs/QP-07-001.md", DOC.as_bytes()),
        Some(DocKey::original("QP-07-001"))
    );
    // Named after another id: no document (the lint's id_not_filename).
    assert_eq!(document_key("docs/QP-07-002.md", DOC.as_bytes()), None);
    // No doctype (an ordinary Markdown file, an older `spec:` file).
    assert_eq!(document_key("README.md", b"# Readme\n"), None);
    assert_eq!(
        document_key("specs/S-1.md", b"---\nspec: S-1\n---\n# S\n"),
        None
    );
    let en = "---\ndoctype: qms.procedure\nid: QP-07-001\ntitle: T\nowner: qa\nlang: en\ntranslation_of: QP-07-001\n---\n";
    assert_eq!(
        document_key("docs/QP-07-001.en.md", en.as_bytes()),
        Some(DocKey {
            doc_id: "QP-07-001".into(),
            lang: Some("en".into())
        })
    );
    assert_eq!(document_key("docs/QP-07-001.id.md", en.as_bytes()), None);
}

#[test]
fn only_md_files_whose_blob_differs_are_changed() {
    let base = listing(vec![
        blob("a/QP-1.md", '1'),
        blob("a/QP-2.md", '2'),
        blob("a/QP-3.md", '3'),
        blob("src/main.rs", '4'),
    ]);
    let head = listing(vec![
        blob("a/QP-1.md", '1'),
        blob("a/QP-2.md", '9'),
        blob("a/QP-4.md", '5'),
        blob("src/main.rs", '6'),
    ]);
    let (changed, removed) = changed_markdown(Some(&base), &head);
    let paths = |l: &BlobListing| l.blobs.iter().map(|b| b.path.clone()).collect::<Vec<_>>();
    assert_eq!(paths(&changed), vec!["a/QP-2.md", "a/QP-4.md"]);
    assert_eq!(paths(&removed), vec!["a/QP-3.md"]);
    // No base: every `.md` of the head, nothing removed.
    let (all, none) = changed_markdown(None, &head);
    assert_eq!(all.blobs.len(), 3);
    assert!(none.blobs.is_empty());
}

#[test]
fn a_second_revision_of_a_document_is_refused_at_open() {
    let open = vec![OpenRevision {
        key: DocKey::original("QP-07-001"),
        merge_request: MergeRequestId::new(4),
        iid: 12,
        title: "revise".into(),
        author: UserId::new(3),
        since: "2026-11-23T01:00:00.000000Z".into(),
        head_sha: "a".repeat(40),
    }];
    let e = refusal(ProjectId::new(7), &open, &[DocKey::original("QP-07-001")]).expect("refused");
    assert_eq!(e.code(), ErrorCode::DocRevisionOpen);
    assert_eq!(e.code().id(), "FG-DOC-0006");
    let ctx = e.context();
    assert!(
        ctx.iter().any(|(k, v)| *k == "holder_iid" && v == "12"),
        "{ctx:?}"
    );
    assert!(ctx.iter().any(|(k, v)| *k == "doc_id" && v == "QP-07-001"));
    // Another document, or the translation of it: no refusal.
    let other = DocKey {
        doc_id: "QP-07-001".into(),
        lang: Some("en".into()),
    };
    assert!(refusal(ProjectId::new(7), &open, &[other]).is_none());
    assert!(refusal(ProjectId::new(7), &[], &[DocKey::original("QP-07-001")]).is_none());
}

/// Review MEDIUM: the wait for a project's revision lock is bounded; past
/// it the create is refused (`FG-DOC-0009 reason=lock_busy`, a-burst
/// 2026-11-28: it used to go on without the rule). Locks nobody holds are
/// dropped from the map.
#[tokio::test]
async fn a_busy_revision_lock_fails_closed_and_idle_locks_are_dropped() {
    let t = forge_db::test_support::FreshDb::migrated().await;
    let revisions = DocRevisions::new(t.db.clone());
    let held = revisions.serialize(ProjectId::new(1)).await.expect("free");
    let started = std::time::Instant::now();
    let busy = revisions
        .serialize(ProjectId::new(1))
        .await
        .expect_err("busy");
    assert!(started.elapsed() >= LOCK_WAIT);
    assert_eq!(busy.code(), ErrorCode::DocRevisionUnchecked);
    assert_eq!(busy.code().id(), "FG-DOC-0009");
    assert!(
        busy.context()
            .iter()
            .any(|(k, v)| *k == "reason" && v == "lock_busy"),
        "{busy:?}"
    );
    drop(held);
    let other = revisions.serialize(ProjectId::new(2)).await;
    assert!(other.is_ok());
    let projects: Vec<ProjectId> = revisions
        .locks
        .lock()
        .expect("map")
        .keys()
        .copied()
        .collect();
    assert_eq!(projects, vec![ProjectId::new(2)]);
}

/// Review of the fail-closed rule (2026-11-28): a git failure a retry may
/// not see again refuses the create retryably (FG-DOC-0009 `reason=git`);
/// one that says the repository cannot be read stays permanent.
#[test]
fn transient_git_failures_are_the_ones_a_retry_may_not_see() {
    for code in [
        ErrorCode::GitTimeout,
        ErrorCode::GitBusy,
        ErrorCode::GitUnavailable,
        ErrorCode::GitBatchFailed,
    ] {
        assert!(transient_git(&ForgeError::new(code)), "{code:?}");
    }
    for code in [
        ErrorCode::GitProcessFailed,
        ErrorCode::Internal,
        ErrorCode::DbUnavailable,
    ] {
        assert!(!transient_git(&ForgeError::new(code)), "{code:?}");
    }
}
