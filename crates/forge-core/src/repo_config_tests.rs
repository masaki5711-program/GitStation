#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly
//! `.forge/repo.yml` (M3 X1, plan 03 §4.4, ADR-0023 Amendment X1).

use super::*;

fn ok(text: &str) -> RepoConfig {
    match parse_repo_config(text.as_bytes()) {
        Ok(c) => c,
        Err(p) => panic!("expected a valid file, got {p:?} for {text:?}"),
    }
}

fn bad(text: &str) -> RepoConfigProblem {
    match parse_repo_config(text.as_bytes()) {
        Ok(c) => panic!("expected a problem, got {c:?} for {text:?}"),
        Err(p) => p,
    }
}

#[test]
fn the_plan_example_parses_with_lines_pins_and_scopes() {
    let c = ok("# .forge/repo.yml\n\
         specs:\n\
         \x20 - repo: specs/zeva-engine      # shared\n\
         \x20   version: v1.4\n\
         \x20   scope: [ZEVA-CFG-*]\n\
         \x20 - repo: Specs/Other\n\
         \x20   track: main\n\
         \x20 - path: spec/\n");
    assert_eq!(c.specs.len(), 3);
    let first = &c.specs[0];
    assert_eq!(first.line, 3);
    match &first.source {
        SpecSource::Repo { written, repo, pin } => {
            assert_eq!(written, "specs/zeva-engine");
            assert_eq!(repo.full_path(), "specs/zeva-engine");
            assert_eq!(pin.kind(), "version");
            assert_eq!(pin.name(), "v1.4");
            assert_eq!(pin.ref_name().as_str(), "refs/tags/v1.4");
        }
        SpecSource::Path(_) => panic!("a repo entry"),
    }
    assert_eq!(
        first
            .scope
            .iter()
            .map(ScopePattern::as_str)
            .collect::<Vec<_>>(),
        ["ZEVA-CFG-*"]
    );
    match &c.specs[1].source {
        SpecSource::Repo { written, repo, pin } => {
            // Written as is; looked up folded (ADR-0015).
            assert_eq!(written, "Specs/Other");
            assert_eq!(repo.full_path(), "specs/other");
            assert_eq!(pin.kind(), "track");
            assert_eq!(pin.ref_name().as_str(), "refs/heads/main");
        }
        SpecSource::Path(_) => panic!("a repo entry"),
    }
    assert_eq!(c.specs[1].line, 6);
    assert!(c.specs[1].scope.is_empty());
    match &c.specs[2].source {
        SpecSource::Path(p) => assert_eq!(p.as_str(), "spec/"),
        SpecSource::Repo { .. } => panic!("a path entry"),
    }
}

#[test]
fn an_empty_file_or_one_without_specs_declares_nothing() {
    assert!(ok("").specs.is_empty());
    assert!(ok("# only a comment\n").specs.is_empty());
    assert!(ok("specs: []\n").specs.is_empty());
}

#[test]
fn scalars_are_text_never_numbers() {
    // YAML would read 1.10 as the float 1.1: a version is the text written.
    let c = ok("specs:\n  - repo: a/b\n    version: 1.10\n");
    match &c.specs[0].source {
        SpecSource::Repo { pin, .. } => assert_eq!(pin.name(), "1.10"),
        SpecSource::Path(_) => panic!("a repo entry"),
    }
    let c = ok("specs:\n  - repo: \"a/b\"\n    version: 'v2'\n");
    assert_eq!(c.specs.len(), 1);
}

#[test]
fn the_review_section_is_reserved_and_not_read() {
    // ADR-0017 §2.7 defines `review:`; X1 accepts it unread.
    let c = ok("review:\n  ai_approval:\n    categories: [docs_only]\nspecs: []\n");
    assert!(c.specs.is_empty());
}

#[test]
fn scope_patterns_match_requirement_ids() {
    let p = ScopePattern::parse("ZEVA-CFG-*").expect("pattern");
    assert!(p.matches("ZEVA-CFG-012"));
    assert!(!p.matches("ZEVA-NET-001"));
    let exact = ScopePattern::parse("ZEVA-CFG-012").expect("pattern");
    assert!(exact.matches("ZEVA-CFG-012"));
    assert!(!exact.matches("ZEVA-CFG-0120"));
    assert!(ScopePattern::parse("*").expect("all").matches("ANY-1"));
    for wrong in [
        "",
        "zeva-cfg-*",
        "ZEVA*CFG",
        "ZEVA-**",
        "-ZEVA",
        "ZEVA CFG",
        "ZEVA/1",
    ] {
        assert!(ScopePattern::parse(wrong).is_none(), "{wrong:?}");
    }
}

/// Every refusal names its rule and the line it is on (1-based; 0 for the
/// whole file).
#[test]
fn an_invalid_file_is_one_problem_with_a_line_and_a_reason() {
    let cases: &[(&str, usize, &str)] = &[
        ("specs: [\n", 2, "syntax"),
        ("- a\n- b\n", 1, "not_a_mapping"),
        ("specs:\n  repo: a/b\n", 2, "specs_not_a_list"),
        ("kind: spec\n", 1, "unknown_key"),
        ("specs: []\nspecs: []\n", 2, "duplicate_key"),
        (
            "specs:\n  - repo: a/b\n    version: v1\n    bogus: 1\n",
            4,
            "unknown_key",
        ),
        ("specs:\n  - version: v1\n", 2, "neither_repo_nor_path"),
        (
            "specs:\n  - repo: a/b\n    path: spec/\n",
            2,
            "repo_and_path",
        ),
        ("specs:\n  - repo: a/b\n", 2, "no_version"),
        (
            "specs:\n  - repo: a/b\n    version: v1\n    track: main\n",
            2,
            "version_and_track",
        ),
        (
            "specs:\n  - path: spec/\n    version: v1\n",
            2,
            "version_on_path",
        ),
        (
            "specs:\n  - repo: a//b\n    version: v1\n",
            2,
            "repo_invalid",
        ),
        (
            "specs:\n  - repo: ../x\n    version: v1\n",
            2,
            "repo_invalid",
        ),
        (
            "specs:\n  - repo: a/b\n    version: \"v1..2\"\n",
            3,
            "version_invalid",
        ),
        (
            "specs:\n  - repo: a/b\n    version: refs/tags/v1\n",
            3,
            "version_invalid",
        ),
        (
            "specs:\n  - repo: a/b\n    version: -v1\n",
            3,
            "version_invalid",
        ),
        (
            "specs:\n  - repo: a/b\n    version:\n",
            3,
            "version_invalid",
        ),
        (
            "specs:\n  - repo: a/b\n    track: \"a b\"\n",
            3,
            "track_invalid",
        ),
        (
            "specs:\n  - repo: a/b\n    version: v1\n    scope: [bad]\n",
            4,
            "scope_invalid",
        ),
        (
            "specs:\n  - repo: a/b\n    version: v1\n    scope: X-*\n",
            4,
            "scope_not_a_list",
        ),
        ("specs:\n  - path: /etc\n", 2, "path_invalid"),
        ("specs:\n  - path: a/../b\n", 2, "path_invalid"),
        ("specs:\n  - path: \"\"\n", 2, "path_invalid"),
        // Review X1 (LOW 5): an empty or null value is the field's own
        // `*_invalid`, never the text "~".
        ("specs:\n  - path:\n", 2, "path_invalid"),
        ("specs:\n  - path: ~\n", 2, "path_invalid"),
        ("specs:\n  - path: null\n", 2, "path_invalid"),
        ("specs:\n  - repo:\n    version: v1\n", 2, "repo_invalid"),
        ("specs:\n  - repo: a/b\n    track:\n", 3, "track_invalid"),
        (
            "specs:\n  - repo: a/b\n    version: ~\n",
            3,
            "version_invalid",
        ),
        // Review X1 (LOW 7): the browse path rules, plus no drive letter,
        // no leading `:` (pathspec magic) and no backslash.
        ("specs:\n  - path: \"C:/spec\"\n", 2, "path_invalid"),
        ("specs:\n  - path: \"c:\"\n", 2, "path_invalid"),
        ("specs:\n  - path: \":(glob)spec\"\n", 2, "path_invalid"),
        ("specs:\n  - path: \"a\\\\b\"\n", 2, "path_invalid"),
        ("specs:\n  - path: \"a\\tb\"\n", 2, "path_invalid"),
        ("specs:\n  - repo: [a]\n    version: v1\n", 2, "not_text"),
        ("specs:\n  - 3\n", 2, "entry_not_a_mapping"),
        (
            "specs:\n  - repo: a/b\n    version: v1\n  - repo: A/B\n    version: v2\n",
            4,
            "duplicate_repo",
        ),
        (
            "specs:\n  - path: spec/\n  - path: spec/\n",
            3,
            "duplicate_path",
        ),
        ("a: &x 1\n", 1, "yaml_anchor"),
        ("specs: &x []\nreview: *x\n", 1, "yaml_anchor"),
        ("specs: !!seq []\n", 1, "yaml_tag"),
        ("specs: []\n---\nspecs: []\n", 2, "multiple_documents"),
        ("? [a]\n: 1\n", 1, "key_not_text"),
    ];
    for (text, line, reason) in cases {
        let p = bad(text);
        assert_eq!((p.line, p.reason), (*line, *reason), "{text:?}");
    }
}

#[test]
fn the_limits_are_problems_not_truncations() {
    let big = format!("# {}\n", "x".repeat(MAX_REPO_CONFIG_BYTES));
    assert_eq!(bad(&big).reason, "too_large");
    assert_eq!(bad(&big).line, 0);

    let mut many = String::from("specs:\n");
    for i in 0..=MAX_SPEC_REFS {
        many.push_str(&format!("  - repo: g/p{i}\n    version: v1\n"));
    }
    let p = bad(&many);
    assert_eq!(p.reason, "too_many_specs");

    let scopes: Vec<String> = (0..=MAX_SCOPE_PATTERNS).map(|i| format!("A-{i}")).collect();
    let text = format!(
        "specs:\n  - repo: a/b\n    version: v1\n    scope: [{}]\n",
        scopes.join(", ")
    );
    assert_eq!(bad(&text).reason, "too_many_scopes");

    let deep = format!("review: {}{}\n", "[".repeat(40), "]".repeat(40));
    assert_eq!(bad(&deep).reason, "too_deep");

    assert_eq!(
        parse_repo_config(b"specs: [\xff]\n").map(|_| ()),
        Err(RepoConfigProblem {
            line: 0,
            reason: "not_utf8"
        })
    );
}

#[test]
fn a_path_is_a_tree_path_as_written() {
    for good in ["spec", "spec/", "docs/spec", "spec:v2", "~notes", "仕様"] {
        let c = ok(&format!("specs:\n  - path: \"{good}\"\n"));
        match &c.specs[0].source {
            SpecSource::Path(p) => assert_eq!(p.as_str(), good),
            SpecSource::Repo { .. } => panic!("a path entry"),
        }
    }
}

/// Review X1 (LOW 6): a mapping holds at most `MAX_MAP_KEYS` keys, and
/// duplicates are found in one pass.
#[test]
fn a_mapping_with_too_many_keys_is_refused() {
    let keys: Vec<String> = (0..=MAX_MAP_KEYS).map(|i| format!("k{i}: 1")).collect();
    let text = format!("review: {{{}}}\n", keys.join(", "));
    assert_eq!(bad(&text).reason, "too_many_keys");
    let keys: Vec<String> = (0..MAX_MAP_KEYS).map(|i| format!("k{i}: 1")).collect();
    assert!(ok(&format!("review: {{{}}}\n", keys.join(", ")))
        .specs
        .is_empty());
    let dup = format!("review: {{{}, k3: 2}}\n", keys[..10].join(", "));
    assert_eq!(bad(&dup).reason, "duplicate_key");
}

proptest::proptest! {
    /// Any bytes: a config or a problem, never a panic (the input is a
    /// repository's file, i.e. anyone who can push).
    #[test]
    fn arbitrary_input_never_panics(bytes in proptest::collection::vec(proptest::num::u8::ANY, 0..512)) {
        let _ = parse_repo_config(&bytes);
    }

    #[test]
    fn arbitrary_yaml_like_text_never_panics(text in "[a-z:\\-\\[\\]{}&*!|>'\" \\n#?,]{0,200}") {
        let _ = parse_repo_config(text.as_bytes());
    }
}
