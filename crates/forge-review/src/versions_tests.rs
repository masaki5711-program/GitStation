#![allow(clippy::expect_used)] // tests may fail loudly

use super::*;
use forge_db::review::StoredVersion;

fn oid(c: char) -> ObjectId {
    ObjectId::parse(&c.to_string().repeat(40)).expect("oid")
}

fn version(n: i64, head: char, purged: bool) -> StoredVersion {
    StoredVersion {
        version_no: n,
        head_sha: oid(head),
        pushed_by: None,
        refs_purged: purged,
        base_sha: None,
        merge_base_sha: None,
        created_at: "2026-11-02T00:00:00Z".to_owned(),
    }
}

fn snapshot(head: Option<char>, versions: Vec<StoredVersion>) -> VersionsSnapshot {
    VersionsSnapshot {
        merge_request: MergeRequestId::new(1),
        iid: 4,
        head_sha: head.map(oid),
        versions,
    }
}

fn name(raw: &str) -> RefName {
    RefName::parse(raw).expect("ref")
}

#[test]
fn missing_version_and_head_refs_are_created() {
    let plan = plan_ref_changes(
        &snapshot(
            Some('b'),
            vec![version(1, 'a', false), version(2, 'b', false)],
        ),
        &BTreeMap::from([(name("refs/merge-requests/4/v1"), oid('a'))]),
    )
    .expect("plan");
    assert_eq!(
        plan.changes,
        [
            RefChange::Create {
                name: name("refs/merge-requests/4/v2"),
                new: oid('b')
            },
            RefChange::Create {
                name: name("refs/merge-requests/4/head"),
                new: oid('b')
            },
        ]
    );
    assert!(plan.reclaimed.is_empty());
}

/// A rerun: every ref already holds its value — success, nothing to do
/// (never a lost compare-and-swap).
#[test]
fn refs_already_in_place_need_no_change() {
    let plan = plan_ref_changes(
        &snapshot(Some('a'), vec![version(1, 'a', false)]),
        &BTreeMap::from([
            (name("refs/merge-requests/4/v1"), oid('a')),
            (name("refs/merge-requests/4/head"), oid('a')),
            // A reserved name below the prefix is left alone.
            (name("refs/merge-requests/4/merge"), oid('f')),
        ]),
    )
    .expect("plan");
    assert_eq!(plan, RefPlan::default());
}

/// A `v<n>` holding a value no row claims is taken back by CAS on the read
/// value; `head` moves by CAS too (not counted as reclaimed).
#[test]
fn an_unclaimed_version_ref_is_reclaimed_by_compare_and_swap() {
    let plan = plan_ref_changes(
        &snapshot(Some('b'), vec![version(1, 'b', false)]),
        &BTreeMap::from([
            (name("refs/merge-requests/4/v1"), oid('e')),
            (name("refs/merge-requests/4/head"), oid('a')),
        ]),
    )
    .expect("plan");
    assert_eq!(plan.reclaimed, [name("refs/merge-requests/4/v1")]);
    assert_eq!(
        plan.changes,
        [
            RefChange::Update {
                name: name("refs/merge-requests/4/v1"),
                new: oid('b'),
                old: oid('e')
            },
            RefChange::Update {
                name: name("refs/merge-requests/4/head"),
                new: oid('b'),
                old: oid('a')
            },
        ]
    );
}

/// ADR-0024 closing table #42: a purged version's ref is deleted, never
/// written; `head` at a purged version is deleted too.
#[test]
fn purged_version_refs_are_deleted_never_written() {
    let plan = plan_ref_changes(
        &snapshot(
            Some('b'),
            vec![version(1, 'a', true), version(2, 'b', true)],
        ),
        &BTreeMap::from([
            (name("refs/merge-requests/4/v1"), oid('a')),
            (name("refs/merge-requests/4/head"), oid('b')),
        ]),
    )
    .expect("plan");
    assert_eq!(
        plan.changes,
        [
            RefChange::Delete {
                name: name("refs/merge-requests/4/v1"),
                old: oid('a')
            },
            RefChange::Delete {
                name: name("refs/merge-requests/4/head"),
                old: oid('b')
            },
        ]
    );
    // Nothing there: nothing created for them.
    let empty = plan_ref_changes(
        &snapshot(
            Some('b'),
            vec![version(1, 'a', true), version(2, 'b', true)],
        ),
        &BTreeMap::new(),
    )
    .expect("plan");
    assert!(empty.changes.is_empty());
}
