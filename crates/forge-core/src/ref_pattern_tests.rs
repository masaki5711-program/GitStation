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

/// Folding is per character and without context: Greek capital sigma folds
/// to σ wherever it stands (a whole-string lowercase would make it final ς at
/// the end of a word and miss the ref), final ς folds to σ too, and the
/// wildcard may sit next to a multibyte letter. A miss would leave a ref
/// unprotected (ADR-0008).
#[test]
fn patterns_match_unicode_case_without_context() {
    for (pattern, refname) in [
        ("refs/heads/aΣ*", "refs/heads/aΣb"),
        ("refs/heads/aΣ*", "refs/heads/aσb"),
        ("refs/heads/*Σ", "refs/heads/aΣ"),
        ("refs/heads/*Σ", "refs/heads/aς"),
        ("refs/heads/*σ", "refs/heads/aΣ"),
        ("refs/heads/aσ", "refs/heads/aΣ"),
        ("refs/heads/aΣ", "refs/heads/aς"),
        ("refs/heads/aς", "refs/heads/AΣ"),
        ("refs/heads/ΣΣ", "refs/heads/σς"),
        ("refs/heads/İx", "refs/heads/i\u{307}x"),
        ("refs/heads/İ*", "refs/heads/İ"),
        ("refs/heads/ß", "refs/heads/ẞ"),
        ("refs/heads/ẞ*", "refs/heads/ßx"),
        ("refs/heads/k", "refs/heads/\u{212A}"),
        ("refs/heads/\u{212A}", "refs/heads/K"),
        ("refs/heads/é*ü", "refs/heads/ÉxÜ"),
        ("refs/heads/日*", "refs/heads/日本"),
        ("refs/heads/*本", "refs/heads/日本"),
        ("refs/heads/Ä*", "refs/heads/ä"),
    ] {
        assert!(p(pattern).matches(&r(refname)), "{pattern} ~ {refname}");
    }
    for (pattern, refname) in [
        ("refs/heads/日*本", "refs/heads/日"),
        ("refs/heads/ß", "refs/heads/ss"),
        ("refs/heads/i", "refs/heads/İ"),
        ("refs/heads/aΣ*", "refs/heads/a"),
    ] {
        assert!(!p(pattern).matches(&r(refname)), "{pattern} !~ {refname}");
    }
}

#[test]
fn equal_patterns_ignoring_case_share_a_key() {
    assert_eq!(p("refs/heads/aΣ").key(), "refs/heads/aσ");
    assert_eq!(p("refs/heads/aς").key(), p("refs/heads/AΣ").key());
    assert_eq!(p("refs/heads/\u{212A}").key(), p("refs/heads/k").key());
    assert_eq!(p("refs/heads/Main").key(), p("refs/heads/MAIN").key());
    assert_eq!(p("refs/heads/Main").key(), "refs/heads/main");
    assert_ne!(p("refs/heads/main").key(), p("refs/heads/main*").key());
}

/// Letters whose case mapping is not one ASCII letter to another: Greek
/// sigma (final and medial), dotted capital I, sharp s and its capital, the
/// Kelvin sign, accented Latin.
const UNICODE_CASES: &[char] = &[
    'Σ', 'σ', 'ς', 'İ', 'ß', 'ẞ', '\u{212A}', 'k', 'é', 'É', 'Ä', 'ä',
];

/// Characters that make interesting patterns: the separator, the wildcard,
/// git's forbidden characters, dots, dashes, cases and multibyte letters.
fn tail() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            4 => prop::sample::select(vec!['a', 'b', 'Z', '0']),
            1 => prop::sample::select(vec![
                '/', '*', '.', '-', '_', '@', '{', '~', ' ', ':', '\\', '\u{7f}', '\n', 'é', 'K',
            ]),
            1 => prop::sample::select(UNICODE_CASES.to_vec()),
        ],
        0..40,
    )
    .prop_map(|cs| cs.into_iter().collect())
}

/// `s` with each character upper- or lowercased by `choices` (cycled). A
/// character whose uppercase is not a single character (ß -> SS) keeps its
/// case: case-insensitive file systems use simple, one-to-one case mapping.
fn case_variant(s: &str, choices: &[bool]) -> String {
    s.chars()
        .zip(choices.iter().cycle())
        .flat_map(|(c, upper)| {
            let mapped: Vec<char> = if *upper {
                c.to_uppercase().collect()
            } else {
                c.to_lowercase().collect()
            };
            if *upper && mapped.len() != 1 {
                vec![c]
            } else {
                mapped
            }
        })
        .collect()
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
    /// and matches its own literal text in any case: each side gets its own
    /// per-character mix of upper and lower case (a pattern variant must parse
    /// too), so a fold that depends on context or position would show.
    #[test]
    fn accepted_patterns_match_their_own_instances(
        pre in prefix(),
        rest in tail(),
        fill in "[a-zA-Z0-9\u{3a3}\u{3c2}\u{3c3}]{0,3}",
        pattern_case in proptest::collection::vec(any::<bool>(), 1..8),
        ref_case in proptest::collection::vec(any::<bool>(), 1..8),
    ) {
        let raw = format!("{pre}{rest}");
        if let Ok(pattern) = RefPattern::parse(&raw) {
            prop_assert!(raw.starts_with("refs/heads/") || raw.starts_with("refs/tags/"));
            prop_assert!(raw.len() <= 255);
            prop_assert!(raw.matches('*').count() <= 1);
            let instance = raw.replacen('*', &fill, 1);
            if let Ok(refname) = RefName::parse(&instance) {
                prop_assert!(pattern.matches(&refname), "{raw} ~ {instance}");
                // The prefix holds no `*` and stays as it is (it must be lowercase).
                let tail_of = |s: &str| s.get(pre.len()..).unwrap_or_default().to_owned();
                let ref_variant = format!("{pre}{}", case_variant(&tail_of(&instance), &ref_case));
                let pattern_variant = format!("{pre}{}", case_variant(&tail_of(&raw), &pattern_case));
                let other = RefPattern::parse(&pattern_variant);
                prop_assert!(other.is_ok(), "{pattern_variant}");
                if let (Ok(variant), Ok(other)) = (RefName::parse(&ref_variant), other) {
                    prop_assert!(pattern.matches(&variant), "{raw} ~ {ref_variant}");
                    prop_assert!(other.matches(&refname), "{pattern_variant} ~ {instance}");
                    prop_assert!(other.matches(&variant), "{pattern_variant} ~ {ref_variant}");
                    prop_assert_eq!(other.key(), pattern.key());
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
