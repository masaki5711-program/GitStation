//! Plan 16 A9: the update's selection (pure).

use super::*;

fn lock(parts: &[(&str, &str, &str)]) -> Lock {
    let parts: Vec<serde_json::Value> = parts
        .iter()
        .map(|(slug, kind, version)| {
            serde_json::json!({
                "slug": slug, "kind": kind, "version": version,
                "digest": format!("sha256:{}", "0".repeat(64)),
                "key_id": "1", "signature": "AA==", "files": [],
            })
        })
        .collect();
    Lock::parse(
        serde_json::json!({"schema_version": 1, "parts": parts})
            .to_string()
            .as_bytes(),
    )
    .expect("lock")
}

fn stable(slug: &str, kind: &str, version: &str, requires: &[&str]) -> CatalogVersion {
    CatalogVersion {
        slug: slug.to_owned(),
        kind: kind.to_owned(),
        version: version.to_owned(),
        requires: requires.iter().map(|r| (*r).to_owned()).collect(),
    }
}

fn published(pairs: &[(&str, &str)]) -> BTreeSet<(String, String)> {
    pairs
        .iter()
        .map(|(s, v)| ((*s).to_owned(), (*v).to_owned()))
        .collect()
}

fn picked(selection: &[Selected]) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = selection
        .iter()
        .map(|s| (s.slug.clone(), s.version.clone()))
        .collect();
    v.sort();
    v
}

fn pair(s: &str, v: &str) -> (String, String) {
    (s.to_owned(), v.to_owned())
}

#[test]
fn locked_parts_move_to_their_newest_stable_version() {
    let lock = lock(&[
        ("claude-code-v2", "adapter", "1.0.0"),
        ("format-on-save", "hook", "1.0.0"),
    ]);
    let catalog = [
        stable("claude-code-v2", "adapter", "1.0.0", &[]),
        stable("format-on-save", "hook", "1.0.0", &[]),
        stable("format-on-save", "hook", "1.1.0", &[]),
    ];
    let live = published(&[("claude-code-v2", "1.0.0"), ("format-on-save", "1.0.0")]);
    let (selection, lines) = update_selection(&lock, &catalog, &live, None).expect("selection");
    assert_eq!(
        picked(&selection),
        [
            pair("claude-code-v2", "1.0.0"),
            pair("format-on-save", "1.1.0")
        ]
    );
    let hook = lines
        .iter()
        .find(|l| l.slug == "format-on-save")
        .expect("line");
    assert_eq!(hook.locked.as_deref(), Some("1.0.0"));
    assert_eq!(hook.next.as_deref(), Some("1.1.0"));
    assert!(!hook.locked_revoked);
}

#[test]
fn a_revoked_version_without_a_replacement_is_dropped_and_a_newer_beta_is_kept() {
    let lock = lock(&[
        ("claude-code-v2", "adapter", "1.0.0"),
        ("guard", "hook", "1.0.0"),
        ("notes", "skill", "2.0.0-beta.1"),
    ]);
    // guard 1.0.0 was revoked: it is not in the stable catalog any more.
    let catalog = [
        stable("claude-code-v2", "adapter", "1.0.0", &[]),
        stable("notes", "skill", "1.0.0", &[]),
    ];
    let live = published(&[("claude-code-v2", "1.0.0"), ("notes", "2.0.0-beta.1")]);
    let (selection, lines) = update_selection(&lock, &catalog, &live, None).expect("selection");
    assert_eq!(
        picked(&selection),
        [
            pair("claude-code-v2", "1.0.0"),
            pair("notes", "2.0.0-beta.1")
        ],
        "never down to the older stable"
    );
    let guard = lines.iter().find(|l| l.slug == "guard").expect("line");
    assert!(guard.locked_revoked);
    assert_eq!(guard.next, None);
}

#[test]
fn the_generation_migration_swaps_the_base_model_and_follows_its_requirements() {
    let lock = lock(&[
        ("claude-code-tmux", "adapter", "1.0.0"),
        ("format-on-save", "hook", "1.0.0"),
    ]);
    let catalog = [
        stable("claude-code-tmux", "adapter", "1.0.0", &[]),
        stable(
            "claude-code-v2",
            "adapter",
            "2.0.0",
            &["session-handoff@^1"],
        ),
        stable("format-on-save", "hook", "1.0.0", &[]),
        stable("session-handoff", "hook", "1.2.0", &[]),
    ];
    let live = published(&[("claude-code-tmux", "1.0.0"), ("format-on-save", "1.0.0")]);
    let (selection, lines) =
        update_selection(&lock, &catalog, &live, Some("claude-code-v2")).expect("selection");
    assert_eq!(
        picked(&selection),
        [
            pair("claude-code-v2", "2.0.0"),
            pair("format-on-save", "1.0.0"),
            pair("session-handoff", "1.2.0"),
        ]
    );
    let old = lines
        .iter()
        .find(|l| l.slug == "claude-code-tmux")
        .expect("line");
    assert_eq!(old.next, None, "the old base model is left");
    let new = lines
        .iter()
        .find(|l| l.slug == "claude-code-v2")
        .expect("line");
    assert_eq!(new.locked, None);
    assert_eq!(new.kind, "adapter");
}

#[test]
fn an_unknown_base_model_is_unresolved() {
    let lock = lock(&[("claude-code-v2", "adapter", "1.0.0")]);
    let catalog = [stable("claude-code-v2", "adapter", "1.0.0", &[])];
    let live = published(&[("claude-code-v2", "1.0.0")]);
    let problems = update_selection(&lock, &catalog, &live, Some("missing-model")).unwrap_err();
    assert_eq!(problems[0].reason, "adapter_missing");
}

#[test]
fn the_merge_request_text_names_the_revoked_part() {
    let asked = AgentUpdateRequest {
        project: ProjectId::new(1),
        origin: AgentUpdateOrigin::Revocation,
        requested_by: None,
        adapter: None,
        revoked: Some(("guard".to_owned(), "1.0.0".to_owned())),
    };
    assert_eq!(
        title_of(&asked),
        "Replace the revoked agent part guard@1.0.0"
    );
    assert_eq!(reason_of(&not_needed("not_in_lock")), Some("not_in_lock"));
    assert_eq!(reason_of(&not_opened("no_author")), None);
}
