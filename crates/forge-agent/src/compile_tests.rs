#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // tests may fail loudly
// Plan 16 A3 review: tests sign with a synthetic seed (clippy.toml).
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;

use forge_errors::ErrorCode;

use crate::*;

const SEED: [u8; 32] = [7u8; 32];

fn keys() -> KeyRing {
    let public = public_key_from_seed(&SEED).unwrap();
    KeyRing::parse(format!(r#"{{"keys":[{{"kid":"1","public_key":"{public}"}}]}}"#).as_bytes())
        .unwrap()
}

fn toml_list(items: &[&str]) -> String {
    items
        .iter()
        .map(|i| format!("\"{i}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A signed part version as the database holds it.
fn part(
    slug: &str,
    kind: &str,
    version: &str,
    requires: &[&str],
    files: &[(&str, &str)],
) -> SourcePart {
    let paths: Vec<&str> = files.iter().map(|(p, _)| *p).collect();
    let manifest = format!(
        "slug = \"{slug}\"\nkind = \"{kind}\"\nversion = \"{version}\"\nrequires = [{}]\nfiles = [{}]\n",
        toml_list(requires),
        toml_list(&paths)
    );
    let mut entries = vec![TreeEntry::file(MANIFEST, manifest)];
    for (p, c) in files {
        entries.push(TreeEntry::file(p, *c));
    }
    let parsed = parse_part(&entries).unwrap();
    let signature = sign_digest(&SEED, &parsed.digest).unwrap();
    SourcePart {
        slug: slug.to_owned(),
        kind: kind.to_owned(),
        version: version.to_owned(),
        requires: requires.iter().map(|r| (*r).to_owned()).collect(),
        capabilities: Vec::new(),
        digest: parsed.digest.to_string(),
        key_id: "1".to_owned(),
        signature,
        files: files
            .iter()
            .map(|(p, c)| ((*p).to_owned(), c.as_bytes().to_vec()))
            .collect(),
        selected_by: "rule".to_owned(),
        reason: Some("base_model".to_owned()),
    }
}

const BASE_SETTINGS: &str = r#"{"model":"opus","hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"node .claude/hooks/guard.js"}]}]}}"#;
const FMT_SETTINGS: &str = r#"{"hooks":{"PostToolUse":[{"hooks":[{"type":"command","command":"node .claude/hooks/fmt.js"}]}],"PreToolUse":[{"matcher":"Edit","hooks":[{"type":"command","command":"node .claude/hooks/fmt.js"}]}]}}"#;

fn adapter() -> SourcePart {
    part(
        "claude-code-v2",
        "adapter",
        "1.0.0",
        &["format-on-save@^1"],
        &[
            ("CLAUDE.md", "# Project\n"),
            (".claude/settings.json", BASE_SETTINGS),
            (".claude/hooks/guard.js", "process.exit(0);\n"),
        ],
    )
}

fn fmt_hook() -> SourcePart {
    part(
        "format-on-save",
        "hook",
        "1.2.0",
        &[],
        &[
            (".claude/settings.json", FMT_SETTINGS),
            (".claude/hooks/fmt.js", "process.exit(0);\n"),
        ],
    )
}

fn input<'a>(parts: &'a [SourcePart], keys: &'a KeyRing) -> CompileInput<'a> {
    CompileInput {
        project_id: 12,
        parts,
        custom: &[],
        keys,
    }
}

fn tree_of(c: &Compiled) -> BTreeMap<String, Vec<u8>> {
    c.written()
        .into_iter()
        .filter(|(p, _)| is_managed_path(p))
        .collect()
}

fn verify(c: &Compiled, tree: &BTreeMap<String, Vec<u8>>) -> LockReport {
    verify_lock(&Lock::parse(&c.lock).unwrap(), &keys(), tree)
}

fn reasons(problems: &[Problem]) -> Vec<(&'static str, &'static str)> {
    problems.iter().map(|p| (p.code.id(), p.reason)).collect()
}

#[test]
fn compile_is_deterministic() {
    let k = keys();
    let a = compile(&input(&[adapter(), fmt_hook()], &k)).unwrap();
    let b = compile(&input(&[fmt_hook(), adapter()], &k)).unwrap();
    assert_eq!(a, b, "the order of the selection changes nothing");
    assert!(a.lock_digest.starts_with("sha256:"));
    // The generated tree verifies against its own lock.
    let report = verify(&a, &tree_of(&a));
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    assert_eq!(report.parts, 2);
}

#[test]
fn shared_settings_are_merged_in_slug_order() {
    let k = keys();
    let c = compile(&input(&[adapter(), fmt_hook()], &k)).unwrap();
    let merged: serde_json::Value =
        serde_json::from_slice(c.files.get(".claude/settings.json").unwrap()).unwrap();
    assert_eq!(merged["model"], "opus");
    let pre = merged["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 2, "arrays are concatenated: {pre:?}");
    assert_eq!(pre[0]["matcher"], "Bash", "claude-code-v2 sorts first");
    assert_eq!(merged["hooks"]["PostToolUse"].as_array().unwrap().len(), 1);
    // The lock keeps each source's text for the verifiers.
    let lock: serde_json::Value = serde_json::from_slice(&c.lock).unwrap();
    let sources: Vec<&str> = lock["parts"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["files"].as_array().unwrap())
        .filter_map(|f| f["content"].as_str())
        .collect();
    assert_eq!(sources, [BASE_SETTINGS, FMT_SETTINGS]);
}

#[test]
fn lock_check_fails_on_hand_edited_hook() {
    let k = keys();
    let c = compile(&input(&[adapter(), fmt_hook()], &k)).unwrap();
    let mut tree = tree_of(&c);
    tree.insert(
        ".claude/hooks/fmt.js".to_owned(),
        b"require('child_process').exec('curl x');\n".to_vec(),
    );
    let r = verify(&c, &tree);
    assert_eq!(
        reasons(&r.problems),
        [("FG-AGENT-0007", "file_changed")],
        "{:?}",
        r.problems
    );
}

#[test]
fn lock_check_fails_on_hand_edited_merged_settings() {
    let k = keys();
    let c = compile(&input(&[adapter(), fmt_hook()], &k)).unwrap();
    let mut tree = tree_of(&c);
    let edited = String::from_utf8(tree[".claude/settings.json"].clone())
        .unwrap()
        .replace("node .claude/hooks/fmt.js", "curl http://evil");
    tree.insert(".claude/settings.json".to_owned(), edited.into_bytes());
    let r = verify(&c, &tree);
    assert_eq!(reasons(&r.problems), [("FG-AGENT-0007", "file_changed")]);
}

#[test]
fn a_merged_source_rewritten_in_the_lock_is_tampering() {
    let k = keys();
    let c = compile(&input(&[adapter(), fmt_hook()], &k)).unwrap();
    // Rewrite one source text and the merged file to match it: the text no
    // longer has the SHA-256 the part's signed digest pins.
    let evil = FMT_SETTINGS.replace("node .claude/hooks/fmt.js", "curl http://evil");
    let lock = String::from_utf8(c.lock.clone()).unwrap().replace(
        &serde_json::to_string(FMT_SETTINGS).unwrap(),
        &serde_json::to_string(&evil).unwrap(),
    );
    let mut tree = tree_of(&c);
    tree.insert(
        ".claude/settings.json".to_owned(),
        merge_json(
            ".claude/settings.json",
            &[
                ("claude-code-v2", BASE_SETTINGS.as_bytes()),
                ("format-on-save", evil.as_bytes()),
            ],
        )
        .unwrap(),
    );
    tree.insert(LOCK_PATH.to_owned(), lock.clone().into_bytes());
    let r = verify_lock(&Lock::parse(lock.as_bytes()).unwrap(), &k, &tree);
    assert_eq!(
        reasons(&r.problems),
        [("FG-AGENT-0002", "merge_source_mismatch")],
        "{:?}",
        r.problems
    );
}

#[test]
fn compile_refuses_conflicting_paths() {
    let k = keys();
    let other = part(
        "other-rules",
        "rule",
        "1.0.0",
        &[],
        &[(".claude/hooks/fmt.js", "process.exit(1);\n")],
    );
    let err = compile(&input(&[adapter(), fmt_hook(), other], &k)).unwrap_err();
    assert_eq!(reasons(&err), [("FG-AGENT-0008", "path_conflict")]);
}

#[test]
fn compile_refuses_a_custom_file_at_a_part_path() {
    let k = keys();
    let parts = [adapter(), fmt_hook()];
    let custom = [CustomFile {
        path: "CLAUDE.md".to_owned(),
        sha256: "0".repeat(64),
        approved_by: None,
    }];
    let err = compile(&CompileInput {
        project_id: 1,
        parts: &parts,
        custom: &custom,
        keys: &k,
    })
    .unwrap_err();
    assert_eq!(reasons(&err), [("FG-AGENT-0008", "path_conflict")]);
}

#[test]
fn two_different_scalars_are_a_merge_conflict() {
    let k = keys();
    let other = part(
        "fast-model",
        "rule",
        "1.0.0",
        &[],
        &[(".claude/settings.json", r#"{"model":"haiku"}"#)],
    );
    let err = compile(&input(&[adapter(), fmt_hook(), other], &k)).unwrap_err();
    assert_eq!(reasons(&err), [("FG-AGENT-0008", "merge_conflict")]);
    assert!(err[0].subject.as_deref().unwrap().contains("/model"));
}

#[test]
fn exactly_one_adapter_is_required() {
    let k = keys();
    let err = compile(&input(&[fmt_hook()], &k)).unwrap_err();
    assert_eq!(reasons(&err), [("FG-AGENT-0008", "adapter_missing")]);
    let tmux = part(
        "claude-code-tmux",
        "adapter",
        "1.0.0",
        &[],
        &[(".claude/agents/team-lead.md", "# lead\n")],
    );
    let err = compile(&input(&[adapter(), fmt_hook(), tmux], &k)).unwrap_err();
    assert_eq!(reasons(&err), [("FG-AGENT-0008", "adapter_multiple")]);
}

#[test]
fn an_unmet_requirement_is_refused() {
    let k = keys();
    // The adapter requires format-on-save@^1, which is not selected.
    let err = compile(&input(&[adapter()], &k)).unwrap_err();
    assert_eq!(reasons(&err), [("FG-AGENT-0008", "requirement_unmet")]);
    let old = part(
        "format-on-save",
        "hook",
        "0.9.0",
        &[],
        &[(".claude/hooks/fmt.js", "process.exit(0);\n")],
    );
    let err = compile(&input(&[adapter(), old], &k)).unwrap_err();
    assert_eq!(reasons(&err), [("FG-AGENT-0008", "requirement_unmet")]);
}

#[test]
fn a_part_changed_in_the_database_is_refused() {
    let k = keys();
    let mut tampered = fmt_hook();
    tampered.files[1].1 = b"require('child_process');\n".to_vec();
    let err = compile(&input(&[adapter(), tampered], &k)).unwrap_err();
    assert_eq!(reasons(&err), [("FG-AGENT-0002", "digest_mismatch")]);
    let mut unknown = fmt_hook();
    unknown.key_id = "9".to_owned();
    let err = compile(&input(&[adapter(), unknown], &k)).unwrap_err();
    assert_eq!(err[0].code, ErrorCode::SigningKeyUnavailable);
}

#[test]
fn a_single_writer_file_is_copied_byte_for_byte() {
    let k = keys();
    let c = compile(&input(&[adapter(), fmt_hook()], &k)).unwrap();
    assert_eq!(c.files["CLAUDE.md"], b"# Project\n");
    assert_eq!(c.files[".claude/hooks/fmt.js"], b"process.exit(0);\n");
    assert!(!c.files.contains_key(LOCK_PATH));
    assert!(c.written().contains_key(LOCK_PATH));
}

#[test]
fn an_unpinned_extra_file_is_unmanaged() {
    let k = keys();
    let c = compile(&input(&[adapter(), fmt_hook()], &k)).unwrap();
    let mut tree = tree_of(&c);
    tree.insert(".claude/hooks/extra.js".to_owned(), b"x".to_vec());
    let r = verify(&c, &tree);
    assert_eq!(reasons(&r.problems), [("FG-AGENT-0007", "unmanaged_file")]);
}

// ── semver ──

#[test]
fn ranges_follow_the_caret_tilde_and_comparison_rules() {
    let v = |s: &str| Version::parse(s).unwrap();
    let r = |s: &str| Range::parse(s).unwrap();
    assert!(r("^1").matches(&v("1.9.0")));
    assert!(!r("^1").matches(&v("2.0.0")));
    assert!(r("^0.2.3").matches(&v("0.2.9")));
    assert!(!r("^0.2.3").matches(&v("0.3.0")));
    assert!(!r("^0.0.3").matches(&v("0.0.4")));
    assert!(r("~1.2.3").matches(&v("1.2.9")));
    assert!(!r("~1.2.3").matches(&v("1.3.0")));
    assert!(r(">=1.0.0, <1.5.0").matches(&v("1.4.9")));
    assert!(!r(">=1.0.0 <1.5.0").matches(&v("1.5.0")));
    assert!(r("1.2.3").matches(&v("1.4.0")));
    assert!(r("*").matches(&v("7.0.0")));
    // A pre-release only when named exactly.
    assert!(!r("^1").matches(&v("1.5.0-rc.1")));
    assert!(r("=1.5.0-rc.1").matches(&v("1.5.0-rc.1")));
    assert!(v("1.0.0-alpha") < v("1.0.0-alpha.1"));
    assert!(v("1.0.0-rc.1") < v("1.0.0"));
    assert!(v("1.0.0-2") < v("1.0.0-10"));
    for bad in [
        "",
        "^",
        "^x",
        ">=1.0.0-rc",
        "01.0.0",
        "1.2.3.4",
        "^1.2.3-rc",
    ] {
        assert!(Range::parse(bad).is_none(), "{bad}");
    }
}

// ── recommend ──

fn cat(slug: &str, kind: &str, version: &str, requires: &[&str]) -> CatalogVersion {
    CatalogVersion {
        slug: slug.to_owned(),
        kind: kind.to_owned(),
        version: version.to_owned(),
        requires: requires.iter().map(|r| (*r).to_owned()).collect(),
    }
}

#[test]
fn recommend_picks_the_base_model_and_its_requirements_newest_stable() {
    let catalog = [
        cat(
            "claude-code-v2",
            "adapter",
            "1.0.0",
            &["format-on-save@^1", "rules-base"],
        ),
        cat("claude-code-v2", "adapter", "1.1.0", &["format-on-save@^1"]),
        cat("format-on-save", "hook", "1.0.0", &[]),
        cat("format-on-save", "hook", "1.4.0", &["lint@~2.1"]),
        cat("format-on-save", "hook", "2.0.0", &[]),
        cat("lint", "skill", "2.1.5", &[]),
        cat("lint", "skill", "2.2.0", &[]),
        cat("kanflow", "skill", "3.0.0", &[]),
    ];
    let r = recommend(
        &catalog,
        DEFAULT_ADAPTER,
        &["kanflow".to_owned(), "gone".to_owned()],
    )
    .unwrap();
    let got: Vec<(&str, &str, &str)> = r
        .parts
        .iter()
        .map(|p| (p.slug.as_str(), p.version.as_str(), p.reason.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("claude-code-v2", "1.1.0", "base_model"),
            ("format-on-save", "1.4.0", "required_by=claude-code-v2"),
            ("kanflow", "3.0.0", "current_lock"),
            ("lint", "2.1.5", "required_by=format-on-save"),
        ]
    );
    assert_eq!(r.unavailable, ["gone"]);
    assert!(r.parts.iter().all(|p| p.selected_by == "rule"));
}

#[test]
fn recommend_without_the_base_model_is_refused() {
    let catalog = [cat("format-on-save", "hook", "1.0.0", &[])];
    let err = recommend(&catalog, DEFAULT_ADAPTER, &[]).unwrap_err();
    assert_eq!(reasons(&err), [("FG-AGENT-0008", "adapter_missing")]);
    let catalog = [cat("claude-code-v2", "adapter", "1.0.0", &["missing@^1"])];
    let err = recommend(&catalog, DEFAULT_ADAPTER, &[]).unwrap_err();
    assert_eq!(reasons(&err), [("FG-AGENT-0008", "requirement_unmet")]);
}
