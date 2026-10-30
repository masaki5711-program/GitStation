use forge_errors::{ErrorCode, ForgeError};
use proptest::prelude::*;

use crate::{RefName, RefPattern};

fn reason(err: &ForgeError) -> &str {
    err.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map_or("", |(_, v)| v.as_str())
}

fn context<'a>(err: &'a ForgeError, key: &str) -> Option<&'a str> {
    err.context()
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.as_str())
}

fn r(name: &str) -> RefName {
    RefName::parse(name).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn p(pattern: &str) -> RefPattern {
    RefPattern::parse(pattern).unwrap_or_else(|e| panic!("{pattern}: {e}"))
}

/// Each broken rule is FG-REPO-0013 with the catalog's `reason`; the first
/// rule broken wins (length, prefix, wildcard, then check-ref-format). The
/// pattern itself never reaches the error.
#[test]
fn a_protected_ref_pattern_breaking_the_rules_is_rejected() {
    let long = format!("refs/heads/{}", "a".repeat(245));
    assert_eq!(long.len(), 256);
    let cases: &[(&str, &str, Option<&str>)] = &[
        (&long, "too_long", None),
        ("", "prefix", None),
        ("main", "prefix", None),
        ("refs/", "prefix", None),
        ("refs/heads", "prefix", None),
        ("refs/remotes/origin/main", "prefix", None),
        ("refs/notes/x", "prefix", None),
        ("REFS/HEADS/main", "prefix", None),
        ("Refs/heads/main", "prefix", None),
        ("refs/Heads/main", "prefix", None),
        ("/refs/heads/main", "prefix", None),
        (" refs/heads/main", "prefix", None),
        ("refs/heads/**", "wildcard", None),
        ("refs/heads/a*b*", "wildcard", None),
        ("refs/heads/*/x", "wildcard", None),
        ("refs/heads/rel*/x", "wildcard", None),
        ("refs/heads/a*/*", "wildcard", None),
        ("refs/heads/", "check_ref_format", Some("trailing_slash")),
        ("refs/tags/", "check_ref_format", Some("trailing_slash")),
        ("refs/heads/a/", "check_ref_format", Some("trailing_slash")),
        ("refs/heads/a.", "check_ref_format", Some("trailing_dot")),
        ("refs/heads/*.", "check_ref_format", Some("trailing_dot")),
        ("refs/heads//a", "check_ref_format", Some("empty_component")),
        ("refs/heads/a..b", "check_ref_format", Some("double_dot")),
        ("refs/heads/a@{1}", "check_ref_format", Some("at_brace")),
        (
            "refs/heads/a b",
            "check_ref_format",
            Some("forbidden_character"),
        ),
        (
            "refs/heads/a~1",
            "check_ref_format",
            Some("forbidden_character"),
        ),
        (
            "refs/heads/a^",
            "check_ref_format",
            Some("forbidden_character"),
        ),
        (
            "refs/heads/a:b",
            "check_ref_format",
            Some("forbidden_character"),
        ),
        (
            "refs/heads/a?",
            "check_ref_format",
            Some("forbidden_character"),
        ),
        (
            "refs/heads/a[",
            "check_ref_format",
            Some("forbidden_character"),
        ),
        (
            "refs/heads/a\\b",
            "check_ref_format",
            Some("forbidden_character"),
        ),
        (
            "refs/heads/a\nb",
            "check_ref_format",
            Some("forbidden_character"),
        ),
        (
            "refs/heads/a\u{7f}",
            "check_ref_format",
            Some("forbidden_character"),
        ),
        (
            "refs/heads/.hidden",
            "check_ref_format",
            Some("component_starts_with_dot"),
        ),
        (
            "refs/heads/.*",
            "check_ref_format",
            Some("component_starts_with_dot"),
        ),
        (
            "refs/heads/x.lock",
            "check_ref_format",
            Some("component_ends_with_lock"),
        ),
        (
            "refs/heads/*.lock",
            "check_ref_format",
            Some("component_ends_with_lock"),
        ),
        (
            "refs/heads/-rf",
            "check_ref_format",
            Some("component_starts_with_dash"),
        ),
        (
            "refs/heads/-*",
            "check_ref_format",
            Some("component_starts_with_dash"),
        ),
    ];
    for (raw, want, rule) in cases {
        let err = RefPattern::parse(raw).expect_err(&format!("{raw:?}"));
        assert_eq!(err.code(), ErrorCode::ProtectedRefRuleInvalid, "{raw:?}");
        assert_eq!(err.code().http_status(), 400);
        assert_eq!(reason(&err), *want, "{raw:?}");
        assert_eq!(context(&err, "rule"), *rule, "{raw:?}");
        assert_eq!(context(&err, "len"), Some(raw.len().to_string().as_str()));
        if raw.len() > 3 {
            assert!(!err.to_string().contains(raw), "no pattern in {err}");
        }
    }
}

#[test]
fn valid_patterns_are_accepted() {
    let longest = format!("refs/heads/{}", "a".repeat(244));
    assert_eq!(longest.len(), 255);
    let listed = [
        "refs/heads/main",
        "refs/heads/*",
        "refs/tags/*",
        "refs/tags/v*",
        "refs/heads/release/*",
        "refs/heads/release-*",
        "refs/heads/*-stable",
        "refs/heads/feat/x*y",
        "refs/heads/日本語",
        "refs/heads/MAIN",
    ];
    for good in listed.iter().map(|s| (*s).to_owned()).chain([longest]) {
        let pattern = p(&good);
        assert_eq!(pattern.as_str(), good);
        assert_eq!(pattern.to_string(), good);
    }
}

#[test]
fn patterns_match_case_insensitively() {
    // Literal patterns: the whole ref, ignoring case (MAIN and main are one
    // file on a case-insensitive file system).
    for (pattern, refname) in [
        ("refs/heads/main", "refs/heads/main"),
        ("refs/heads/main", "refs/heads/MAIN"),
        ("refs/heads/MAIN", "refs/heads/main"),
        ("refs/heads/Main", "refs/heads/mAIN"),
        ("refs/tags/V1", "refs/tags/v1"),
        ("refs/heads/ÄRGER", "refs/heads/ärger"),
    ] {
        assert!(p(pattern).matches(&r(refname)), "{pattern} ~ {refname}");
    }
    // Wildcards, also ignoring case. `*` matches any run of characters,
    // `/` included and the empty run: broader matching only ever protects
    // more refs (the strictest matching rule wins, ADR-0010 amendment).
    for (pattern, refname) in [
        ("refs/heads/release/*", "refs/heads/release/1.0"),
        ("refs/heads/release/*", "refs/heads/RELEASE/1.0"),
        ("refs/heads/RELEASE/*", "refs/heads/release/x"),
        ("refs/heads/release/*", "refs/heads/release/1.0/hotfix"),
        ("refs/heads/*", "refs/heads/anything/at/all"),
        ("refs/heads/*-stable", "refs/heads/13-STABLE"),
        ("refs/heads/v*", "refs/heads/v"),
        ("refs/tags/v*", "refs/tags/V2.0"),
        ("refs/heads/a*z", "refs/heads/az"),
    ] {
        assert!(p(pattern).matches(&r(refname)), "{pattern} ~ {refname}");
    }
    for (pattern, refname) in [
        ("refs/heads/main", "refs/heads/main2"),
        ("refs/heads/main", "refs/heads/xmain"),
        ("refs/heads/main", "refs/tags/main"),
        ("refs/heads/main", "refs/heads/main/x"),
        ("refs/heads/*", "refs/tags/v1"),
        ("refs/tags/*", "refs/heads/main"),
        ("refs/heads/release/*", "refs/heads/release"),
        ("refs/heads/release/*", "refs/heads/releases/1"),
        ("refs/heads/*-stable", "refs/heads/stable"),
        ("refs/heads/a*z", "refs/heads/a"),
        ("refs/heads/ab*ba", "refs/heads/aba"),
    ] {
        assert!(!p(pattern).matches(&r(refname)), "{pattern} !~ {refname}");
    }
}

#[test]
fn equal_patterns_ignoring_case_share_a_key() {
    assert_eq!(p("refs/heads/Main").key(), p("refs/heads/MAIN").key());
    assert_eq!(p("refs/heads/Main").key(), "refs/heads/main");
    assert_ne!(p("refs/heads/main").key(), p("refs/heads/main*").key());
}

/// Characters that make interesting patterns: the separator, the wildcard,
/// git's forbidden characters, dots, dashes, cases and a multibyte letter.
fn tail() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            4 => prop::sample::select(vec!['a', 'b', 'Z', '0']),
            1 => prop::sample::select(vec![
                '/', '*', '.', '-', '_', '@', '{', '~', ' ', ':', '\\', '\u{7f}', '\n', 'é', 'K',
            ]),
        ],
        0..40,
    )
    .prop_map(|cs| cs.into_iter().collect())
}

fn prefix() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
        "refs/heads/",
        "refs/tags/",
        "refs/heads/release/",
        "refs/",
        "refs/remotes/",
        "REFS/heads/",
        "",
    ])
}

proptest! {
    /// Whatever the parser accepts is a valid ref once the `*` is filled in,
    /// and matches its own literal text in any case.
    #[test]
    fn accepted_patterns_match_their_own_instances(pre in prefix(), rest in tail(), fill in "[a-z0-9]{0,3}") {
        let raw = format!("{pre}{rest}");
        if let Ok(pattern) = RefPattern::parse(&raw) {
            prop_assert!(raw.starts_with("refs/heads/") || raw.starts_with("refs/tags/"));
            prop_assert!(raw.len() <= 255);
            prop_assert!(raw.matches('*').count() <= 1);
            let instance = raw.replacen('*', &fill, 1);
            if let Ok(refname) = RefName::parse(&instance) {
                prop_assert!(pattern.matches(&refname), "{raw} ~ {instance}");
                // The prefix holds no `*`, so it starts the instance too.
                let rest_upper = instance.get(pre.len()..).unwrap_or_default().to_uppercase();
                if let Ok(upper) = RefName::parse(&format!("{pre}{rest_upper}")) {
                    prop_assert!(pattern.matches(&upper), "{raw} ~ upper {instance}");
                }
            }
        }
    }

    /// The prefix and length rules (the part the database CHECK repeats) are
    /// decided before anything else, so a pattern refused for another reason
    /// always has the right prefix and length.
    #[test]
    fn prefix_and_length_are_checked_first(pre in prefix(), rest in tail()) {
        let raw = format!("{pre}{rest}");
        let db_accepts = (raw.starts_with("refs/heads/") || raw.starts_with("refs/tags/"))
            && raw.len() <= 255;
        match RefPattern::parse(&raw) {
            Ok(_) => prop_assert!(db_accepts),
            Err(e) => {
                let why = reason(&e).to_owned();
                prop_assert_eq!(matches!(why.as_str(), "too_long" | "prefix"), !db_accepts, "{}", why);
            }
        }
    }
}
