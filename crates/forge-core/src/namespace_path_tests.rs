use forge_errors::ErrorCode;
use proptest::prelude::*;

use crate::namespace_path::RESERVED_TOP_LEVEL;
use crate::path_vectors::{accepted, rejected};
use crate::PathSegment;

/// An independent statement of the ADR-0015 rules (not the regex the
/// implementation will use).
fn model_accepts(s: &str) -> bool {
    let n = s.chars().count();
    let alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let inner = |c: char| alnum(c) || matches!(c, '.' | '_' | '-');
    let (Some(first), Some(last)) = (s.chars().next(), s.chars().last()) else {
        return false;
    };
    if !(1..=64).contains(&n) || !alnum(first) || !alnum(last) || !s.chars().all(inner) {
        return false;
    }
    if s.contains("..") || [".git", ".atom", ".wiki"].iter().any(|x| s.ends_with(x)) {
        return false;
    }
    let stem = s.split('.').next().unwrap_or_default();
    let device = matches!(stem, "con" | "prn" | "aux" | "nul")
        || ["com", "lpt"].iter().any(|p| {
            stem.strip_prefix(p)
                .is_some_and(|d| matches!(d.as_bytes(), [b'1'..=b'9']))
        });
    !device
}

#[test]
fn path_segment_rules_match_adr_0015() {
    for good in accepted() {
        let segment = PathSegment::parse(&good).unwrap_or_else(|e| panic!("{good:?}: {e}"));
        assert_eq!(segment.as_str(), good);
        assert!(model_accepts(&good), "the model disagrees on {good:?}");
    }
    for bad in rejected() {
        let err = PathSegment::parse(&bad).expect_err(&format!("{bad:?}"));
        assert_eq!(err.code(), ErrorCode::NamespacePathInvalid, "{bad:?}");
        assert_eq!(err.code().http_status(), 400);
        assert!(err.to_string().contains("reason="), "{err}");
        assert!(!model_accepts(&bad), "the model disagrees on {bad:?}");
    }
}

/// Each rule reports the `reason` the catalog (FG-REPO-0007) names; the first
/// rule broken wins.
#[test]
fn each_broken_path_rule_reports_its_catalog_reason() {
    let long = "a".repeat(65);
    let cases = [
        ("", "empty"),
        (long.as_str(), "too_long"),
        ("Acme", "charset"),
        ("a b", "charset"),
        ("a\n", "charset"),
        ("a\u{0663}", "charset"),
        ("-a", "edge"),
        ("a.", "edge"),
        ("a..b", "double_dot"),
        ("x.git", "reserved_suffix"),
        ("x.wiki", "reserved_suffix"),
        ("lpt9.x", "device_name"),
        ("con", "device_name"),
    ];
    for (raw, reason) in cases {
        let err = PathSegment::parse(raw).expect_err(raw);
        assert_eq!(err.code(), ErrorCode::NamespacePathInvalid, "{raw:?}");
        let text = err.to_string();
        assert!(
            text.contains(&format!("reason={reason}")),
            "{raw:?}: {text}"
        );
        assert!(
            text.contains(&format!("len={}", raw.chars().count())),
            "{raw:?}: {text}"
        );
    }
}

/// The rejected name never enters the error (it is user input).
#[test]
fn a_rejected_name_is_not_in_the_error_text() {
    for raw in ["Secretname", "secretname.git", "secret..name"] {
        let text = PathSegment::parse(raw).expect_err(raw).to_string();
        assert!(!text.to_lowercase().contains("secret"), "{raw}: {text}");
    }
    let text = PathSegment::parse_top_level("admin")
        .expect_err("reserved")
        .to_string();
    assert!(!text.contains("admin"), "{text}");
}

proptest! {
    #[test]
    fn path_segment_parse_agrees_with_the_model_on_arbitrary_text(s in "\\PC{0,80}") {
        prop_assert_eq!(PathSegment::parse(&s).is_ok(), model_accepts(&s));
    }

    #[test]
    fn path_segment_parse_accepts_every_well_formed_name(
        s in "[a-z0-9]([a-z0-9._-]{0,62}[a-z0-9])?"
    ) {
        prop_assert_eq!(PathSegment::parse(&s).is_ok(), model_accepts(&s));
        if model_accepts(&s) {
            let parsed = PathSegment::parse(&s).expect("model accepts");
            prop_assert_eq!(parsed.as_str(), s.as_str());
        }
    }
}

#[test]
fn a_reserved_top_level_segment_is_rejected() {
    let mut expected = vec![
        "-",
        "api",
        "admin",
        "assets",
        "login",
        "logout",
        "signup",
        "settings",
        "dashboard",
        "explore",
        "new",
        "search",
        "notifications",
        "docs",
        "help",
        "healthz",
        "readyz",
        "metrics",
        "users",
        "groups",
        "projects",
        "oauth",
        "edge",
        "fleet",
        "devices",
        "favicon.ico",
        "robots.txt",
    ];
    expected.sort_unstable();
    let mut listed: Vec<&str> = RESERVED_TOP_LEVEL.to_vec();
    listed.sort_unstable();
    assert_eq!(listed, expected);
    for word in expected.iter().filter(|w| **w != "-") {
        let err = PathSegment::parse_top_level(word).expect_err(word);
        assert_eq!(err.code(), ErrorCode::NamespacePathReserved, "{word}");
        assert_eq!(err.code().http_status(), 400);
        // Below a group the same word is an ordinary name.
        assert_eq!(PathSegment::parse(word).expect(word).as_str(), *word);
    }
    // Syntax is checked first: "-" is not a valid segment at all.
    let err = PathSegment::parse_top_level("-").expect_err("-");
    assert_eq!(err.code(), ErrorCode::NamespacePathInvalid);
    // Only the exact word is reserved.
    for ok in ["apis", "my-api", "acme", "edge2", "user"] {
        assert_eq!(PathSegment::parse_top_level(ok).expect(ok).as_str(), ok);
    }
    // A syntax failure of a non-reserved name keeps its own code.
    let err = PathSegment::parse_top_level("Acme").expect_err("Acme");
    assert_eq!(err.code(), ErrorCode::NamespacePathInvalid);
}
