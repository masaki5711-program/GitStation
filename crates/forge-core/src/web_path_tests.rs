use std::collections::BTreeSet;

use forge_errors::{ErrorCode, ForgeError};
use proptest::prelude::*;
use serde_json::{json, Value};

use crate::namespace_path::{LookupPath, NamespacePath, MAX_DEPTH};
use crate::web_path::{
    is_invisible, log_safe_path, parse_web_path, GitRoute, InstanceRoute, ParsedPath, RouteWord,
    TailSegments, LOG_PATH_MAX_CHARS, MAX_QUERY_LEN,
};
use crate::{
    GroupId, NamespaceKind, NamespaceOwner, ProjectAccess, ProjectId, Resolved, ResolvedTarget,
    Role, UserId, Visibility,
};

/// The shared vectors (ADR-0015): also read by forge-web and the SPA.
const VECTORS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/url-vectors.json"
));

fn vectors() -> Value {
    serde_json::from_str(VECTORS).expect("url-vectors.json is JSON")
}

fn list<'a>(doc: &'a Value, key: &str) -> &'a Vec<Value> {
    doc[key].as_array().expect(key)
}

fn ctx<'a>(err: &'a ForgeError, key: &str) -> Option<&'a str> {
    err.context()
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.as_str())
}

/// `parse_web_path` refused with FG-WEB-0001 and this `reason`.
fn assert_reason(path: &str, query: Option<&str>, reason: &str) -> ForgeError {
    let err = parse_web_path(path, query)
        .expect_err(&format!("{path:?} {query:?} must be refused ({reason})"));
    assert_eq!(err.code(), ErrorCode::RouteNotFound, "{path:?}: {err}");
    assert_eq!(err.code().http_status(), 404);
    assert_eq!(ctx(&err, "reason"), Some(reason), "{path:?}: {err}");
    err
}

/// The JSON form of a parse result, as the vectors state it.
fn describe(parsed: &ParsedPath) -> Value {
    let rest = |r: &TailSegments| json!(r.as_slice());
    match parsed {
        ParsedPath::NamespaceHome(path) => json!({"kind": "home", "namespace": path.full_path()}),
        ParsedPath::Scoped {
            path,
            word,
            rest: r,
        } => json!({
            "kind": "scoped", "namespace": path.full_path(), "word": word.as_str(), "rest": rest(r)
        }),
        ParsedPath::Git { path, route } => {
            json!({"kind": "git", "namespace": path.full_path(), "route": route.as_str()})
        }
        ParsedPath::Instance { route, rest: r } => {
            json!({"kind": "instance", "route": route.as_str(), "rest": rest(r)})
        }
    }
}

fn path_and_query(v: &Value) -> (&str, Option<&str>) {
    (
        v["path"].as_str().expect("path"),
        v.get("query").map(|q| q.as_str().expect("query")),
    )
}

#[test]
fn url_vectors_accept_and_reject() {
    let doc = vectors();
    assert_eq!(doc["schema"], 1);
    for v in list(&doc, "accept") {
        let (path, query) = path_and_query(v);
        let parsed =
            parse_web_path(path, query).unwrap_or_else(|e| panic!("{} ({path:?}): {e}", v["name"]));
        assert_eq!(describe(&parsed), v["expect"], "{}", v["name"]);
    }
    for v in list(&doc, "reject") {
        let (path, query) = path_and_query(v);
        let reason = v["reason"].as_str().expect("reason");
        let err = assert_reason(path, query, reason);
        // The input never enters the error (it is logged via log_safe_path).
        if path.len() > 4 {
            assert!(!err.to_string().contains(path), "{}: {err}", v["name"]);
        }
        assert!(ctx(&err, "part").is_some(), "{}: {err}", v["name"]);
    }
}

/// The vectors cover what ADR-0015 asks of them, and stay well-formed.
#[test]
fn url_vectors_cover_both_forms_and_every_dot_git_place() {
    let doc = vectors();
    let all: Vec<&Value> = list(&doc, "accept")
        .iter()
        .chain(list(&doc, "reject"))
        .collect();
    let mut names = BTreeSet::new();
    for v in &all {
        let name = v["name"].as_str().expect("name");
        assert!(names.insert(name), "duplicate vector name {name:?}");
        let form = v["form"].as_str().expect("form");
        assert!(["both", "browser", "raw"].contains(&form), "{name}");
        assert_eq!(
            form == "browser",
            v.get("browser_input").is_some(),
            "{name}"
        );
    }
    for form in ["both", "browser", "raw"] {
        for key in ["accept", "reject"] {
            assert!(
                list(&doc, key).iter().any(|v| v["form"] == form),
                "no {form} vector in {key}"
            );
        }
    }
    let reasons: BTreeSet<&str> = list(&doc, "reject")
        .iter()
        .map(|v| v["reason"].as_str().expect("reason"))
        .collect();
    assert_eq!(
        reasons,
        BTreeSet::from(["path_rejected", "service_unsupported", "unknown_route"])
    );
    // The places .git is not allowed (ADR-0015 3-5, R14).
    let dot_git: Vec<&str> = list(&doc, "reject")
        .iter()
        .filter(|v| v["dot_git"] == true)
        .map(|v| v["path"].as_str().expect("path"))
        .collect();
    for needed in [
        "/acme.git/x/info/refs",
        "/acme/x.git",
        "/acme/x.git/-/blob/main/a",
        "/acme/x.git/objects/info/packs",
        "/acme/x.git/HEAD",
        "/acme/x.git.git/info/refs",
    ] {
        assert!(dot_git.contains(&needed), "missing .git vector {needed}");
    }
    // Every route word and git route is exercised by an accepted vector.
    let accepted: Vec<Value> = list(&doc, "accept")
        .iter()
        .map(|v| v["expect"].clone())
        .collect();
    for word in RouteWord::ALL {
        assert!(
            accepted.iter().any(|e| e["word"] == word.as_str()),
            "{word:?}"
        );
    }
    for route in GitRoute::ALL {
        assert!(
            accepted.iter().any(|e| e["route"] == route.as_str()),
            "{route:?}"
        );
    }
    for route in InstanceRoute::ALL {
        assert!(
            accepted.iter().any(|e| e["route"] == route.as_str()),
            "{route:?}"
        );
    }
}

#[test]
fn dot_git_only_on_last_segment_of_smart_http() {
    let q = Some("service=git-upload-pack");
    let ok = parse_web_path("/acme/platform/x.git/info/refs", q).expect("smart http");
    assert_eq!(
        ok,
        ParsedPath::Git {
            path: LookupPath::fold(&["acme", "platform", "x"]).expect("fold"),
            route: GitRoute::InfoRefsUploadPack,
        }
    );
    // Not the last namespace segment, not without a git endpoint, not
    // before /-/, not as dumb HTTP.
    for path in [
        "/acme.git/x/info/refs",
        "/acme.git/platform/x.git/info/refs",
        "/acme/x.git",
        "/acme/x.git/-/blob/main/a",
        "/acme/x.git/objects/info/packs",
        "/acme/x.git/HEAD",
        "/acme/x.git/info",
        "/acme/x.git/info/refs/more",
        "/acme.git/x/-/tree/main",
        "/acme.git/x",
    ] {
        let err = assert_reason(path, q, "path_rejected");
        assert!(
            matches!(ctx(&err, "part"), Some("git" | "namespace")),
            "{path}: {err}"
        );
    }
    // A namespace home or scoped path never carries .git in its namespace.
    assert_reason("/acme/x.GIT", None, "path_rejected");
    // After /-/ the tail is a file or ref name: .git there is just a name.
    let tail = parse_web_path("/acme/x/-/blob/main/lib.git", None).expect("tail");
    assert_eq!(describe(&tail)["rest"], json!(["main", "lib.git"]));
}

#[test]
fn dot_git_dot_git_is_rejected() {
    let q = Some("service=git-upload-pack");
    // .git is removed once; x.git is not a valid name.
    let err = assert_reason("/acme/x.git.git/info/refs", q, "path_rejected");
    assert_eq!(ctx(&err, "rule"), Some("reserved_suffix"), "{err}");
    assert_reason("/acme/x.git.git/git-upload-pack", None, "path_rejected");
    assert_reason("/acme/x.git.git", None, "path_rejected");
    assert_reason("/acme/.git/info/refs", q, "path_rejected");
}

#[test]
fn kelvin_sign_and_dotted_i_never_fold_to_ascii() {
    // The hazard: Unicode lowercasing turns them into ASCII-looking names.
    assert_eq!("\u{212a}".to_lowercase(), "k");
    assert!("\u{130}".to_lowercase().starts_with('i'));
    for raw in [
        "\u{212a}",
        "\u{212a}elvin",
        "\u{130}",
        "\u{130}stanbul",
        "acme\u{212a}",
    ] {
        let err = LookupPath::fold(&[raw]).expect_err(raw);
        assert_eq!(err.code(), ErrorCode::RouteNotFound);
        assert_eq!(ctx(&err, "rule"), Some("non_ascii"), "{raw}: {err}");
        assert_reason(&format!("/{raw}"), None, "path_rejected");
        assert_reason(&format!("/acme/{raw}/-/tree"), None, "path_rejected");
    }
    // Percent-encoded they are refused for the `%` alone.
    for raw in ["%E2%84%AAelvin", "%C4%B0stanbul"] {
        let err = LookupPath::fold(&[raw]).expect_err(raw);
        assert_eq!(ctx(&err, "rule"), Some("percent"), "{raw}: {err}");
    }
    // ASCII capitals do fold.
    let folded = LookupPath::fold(&["KELVIN", "Istanbul"]).expect("ascii");
    assert_eq!(folded.full_path(), "kelvin/istanbul");
}

#[test]
fn route_word_is_case_sensitive() {
    for word in RouteWord::ALL {
        assert_eq!(RouteWord::parse(word.as_str()), Some(*word));
        let upper = word.as_str().to_ascii_uppercase();
        assert_eq!(RouteWord::parse(&upper), None);
        let mut capital = word.as_str().to_owned();
        capital[..1].make_ascii_uppercase();
        assert_eq!(RouteWord::parse(&capital), None);
        assert_reason(&format!("/acme/x/-/{capital}"), None, "unknown_route");
        assert_reason(&format!("/acme/x/-/{upper}/main"), None, "unknown_route");
        let ok = parse_web_path(&format!("/acme/x/-/{}", word.as_str()), None).expect("word");
        assert_eq!(describe(&ok)["word"], word.as_str());
    }
    // The ref and the file keep their case; only the namespace folds.
    let parsed = parse_web_path("/ACME/X/-/blob/Main/A.RS", None).expect("parse");
    assert_eq!(
        describe(&parsed),
        json!({"kind": "scoped", "namespace": "acme/x", "word": "blob", "rest": ["Main", "A.RS"]})
    );
}

#[test]
fn trailing_slash_is_rejected_not_normalised() {
    for path in [
        "/",
        "/acme/",
        "/acme/x/",
        "/acme/x/-/",
        "/acme/x/-/tree/",
        "/acme/x/-/tree/main/",
        "/acme/x.git/",
        "/acme/x.git/info/refs/",
        "/-/",
        "/-/projects/42/",
        "//acme",
        "/acme//x",
        "/acme/x/-/blob//a",
    ] {
        let err = assert_reason(path, Some("service=git-upload-pack"), "path_rejected");
        assert_eq!(ctx(&err, "rule"), Some("empty_segment"), "{path}: {err}");
    }
}

#[test]
fn reserved_top_level_words_are_not_namespaces() {
    for word in crate::namespace_path::RESERVED_TOP_LEVEL {
        if *word == "-" {
            continue;
        }
        let err = assert_reason(&format!("/{word}"), None, "unknown_route");
        assert_eq!(ctx(&err, "part"), Some("reserved_top_level"));
        assert_reason(&format!("/{word}/x/-/tree"), None, "unknown_route");
        // Below the top level the word is an ordinary name.
        let nested = parse_web_path(&format!("/acme/{word}"), None).expect(word);
        assert_eq!(describe(&nested)["namespace"], format!("acme/{word}"));
    }
}

#[test]
fn lookup_path_fold_rules() {
    let cases: &[(&[&str], &str)] = &[
        (&[], "empty"),
        (&["acme", ""], "empty"),
        (&["日本"], "non_ascii"),
        (&["a\u{0}b"], "control"),
        (&["a\tb"], "control"),
        (&["a%41"], "percent"),
        (&["a%2Fb"], "percent"),
        (&["a b"], "charset"),
        (&["-a"], "edge"),
        (&["a..b"], "double_dot"),
        (&["x.git"], "reserved_suffix"),
        (&["con"], "device_name"),
    ];
    for (raw, rule) in cases {
        let err = LookupPath::fold(raw).expect_err(&format!("{raw:?}"));
        assert_eq!(err.code(), ErrorCode::RouteNotFound, "{raw:?}");
        assert_eq!(ctx(&err, "reason"), Some("path_rejected"));
        assert_eq!(ctx(&err, "part"), Some("namespace"));
        assert_eq!(ctx(&err, "rule"), Some(*rule), "{raw:?}: {err}");
    }
    let deep: Vec<String> = (0..=MAX_DEPTH).map(|i| format!("s{i}")).collect();
    let deep: Vec<&str> = deep.iter().map(String::as_str).collect();
    let err = LookupPath::fold(&deep).expect_err("21 deep");
    assert_eq!(ctx(&err, "rule"), Some("too_deep"));
    assert!(LookupPath::fold(&deep[..MAX_DEPTH]).is_ok());
    let a = "a".repeat(64);
    let long = [a.as_str(), a.as_str(), a.as_str(), &"d".repeat(61)];
    let err = LookupPath::fold(&long).expect_err("256 long");
    assert_eq!(ctx(&err, "rule"), Some("path_too_long"));
    let fits = [a.as_str(), a.as_str(), a.as_str(), &"d".repeat(60)];
    assert_eq!(LookupPath::fold(&fits).expect("255").full_path().len(), 255);
    // Folding is ASCII-only and the result is a valid NamespacePath.
    let folded = LookupPath::fold(&["Acme", "Forge-Web"]).expect("fold");
    assert_eq!(folded.to_string(), "acme/forge-web");
    assert_eq!(folded.top_level(), "acme");
    assert_eq!(folded.depth(), 2);
    assert_eq!(
        folded.as_namespace_path(),
        &NamespacePath::parse("acme/forge-web").expect("parse")
    );
}

#[test]
fn namespace_path_parse_and_limits() {
    let path = NamespacePath::parse("acme/platform/x").expect("parse");
    assert_eq!(path.full_path(), "acme/platform/x");
    assert_eq!(path.depth(), 3);
    assert_eq!(path.top_level(), "acme");
    for (raw, reason) in [
        ("", "empty"),
        ("acme/", "empty"),
        ("Acme", "charset"),
        ("a/x.git", "reserved_suffix"),
    ] {
        let err = NamespacePath::parse(raw).expect_err(raw);
        assert_eq!(err.code(), ErrorCode::NamespacePathInvalid, "{raw:?}");
        assert_eq!(ctx(&err, "reason"), Some(reason), "{raw:?}: {err}");
    }
    let deep = vec!["a"; MAX_DEPTH + 1].join("/");
    let err = NamespacePath::parse(&deep).expect_err("deep");
    assert_eq!(ctx(&err, "reason"), Some("too_deep"));
    let long = [
        "a".repeat(64),
        "b".repeat(64),
        "c".repeat(64),
        "d".repeat(61),
    ]
    .join("/");
    let err = NamespacePath::parse(&long).expect_err("long");
    assert_eq!(ctx(&err, "reason"), Some("path_too_long"));
    assert!(NamespacePath::from_segments(Vec::new()).is_err());
}

#[test]
fn tail_segments_decode_once_and_refuse_separators() {
    let tail =
        TailSegments::decode(&["main", "%E6%97%A5%E6%9C%AC.md", "a%20b", "100%25", "a%252F"])
            .expect("decode");
    assert_eq!(tail.as_slice(), ["main", "日本.md", "a b", "100%", "a%2F"]);
    assert_eq!(tail.len(), 5);
    assert!(!tail.is_empty());
    assert_eq!(tail.iter().count(), 5);
    assert!(TailSegments::decode(&[]).expect("empty").is_empty());
    let cases = [
        ("", "empty"),
        ("a%2Fb", "encoded_separator"),
        ("a%2fb", "encoded_separator"),
        ("a%5Cb", "encoded_separator"),
        ("a%5cb", "encoded_separator"),
        ("a\\b", "separator"),
        ("a%00b", "control"),
        ("a\u{0}b", "control"),
        ("a%0Db", "control"),
        ("a%7Fb", "control"),
        ("a%C2%85b", "control"),
        (".", "dot_segment"),
        ("..", "dot_segment"),
        ("%2e", "dot_segment"),
        ("%2E%2e", "dot_segment"),
        ("%FF", "invalid_utf8"),
        ("%C0%AF", "invalid_utf8"),
        ("%E6%97", "invalid_utf8"),
        ("%", "malformed_escape"),
        ("%4", "malformed_escape"),
        ("%zz", "malformed_escape"),
        ("%%41", "malformed_escape"),
        ("a%E2%80%8Bb", "invisible"),
        ("a%E2%80%8Fb", "invisible"),
        ("a%E2%80%A8b", "invisible"),
        ("a%E2%80%AEb", "invisible"),
        ("a%E2%81%A0b", "invisible"),
        ("a%E2%81%A9b", "invisible"),
        ("%EF%BB%BFa", "invisible"),
        ("a%F3%A0%80%81", "invisible"),
        ("a%F3%A0%81%BF", "invisible"),
        ("a\u{202e}b", "invisible"),
    ];
    for (raw, rule) in cases {
        let err = TailSegments::decode(&["main", raw]).expect_err(raw);
        assert_eq!(err.code(), ErrorCode::RouteNotFound, "{raw:?}");
        assert_eq!(ctx(&err, "reason"), Some("path_rejected"));
        assert_eq!(ctx(&err, "part"), Some("tail"));
        assert_eq!(ctx(&err, "rule"), Some(rule), "{raw:?}: {err}");
    }
}

#[test]
fn info_refs_needs_exactly_one_known_service() {
    let path = "/acme/x.git/info/refs";
    let route = |q| {
        let parsed = parse_web_path(path, Some(q)).expect(q);
        let ParsedPath::Git { route, .. } = parsed else {
            panic!("{parsed:?}");
        };
        route
    };
    assert_eq!(
        route("service=git-upload-pack"),
        GitRoute::InfoRefsUploadPack
    );
    assert_eq!(
        route("service=git-receive-pack"),
        GitRoute::InfoRefsReceivePack
    );
    assert_eq!(
        route("a=1&service=git-receive-pack&b"),
        GitRoute::InfoRefsReceivePack
    );
    for (query, rule) in [
        (None, "missing"),
        (Some(""), "missing"),
        (Some("x=1"), "missing"),
        (Some("services=git-upload-pack"), "missing"),
        (
            Some("service=git-upload-pack&service=git-receive-pack"),
            "duplicate",
        ),
        (Some("service=git-upload-pack&service"), "duplicate"),
        (Some("service"), "unknown"),
        (Some("service="), "unknown"),
        (Some("service=GIT-UPLOAD-PACK"), "unknown"),
        (Some("service=git-upload-archive"), "unknown"),
        (Some("service=git%2Dupload-pack"), "unknown"),
        (Some("service=git-upload-pack=x"), "unknown"),
    ] {
        let err = assert_reason(path, query, "service_unsupported");
        assert_eq!(ctx(&err, "rule"), Some(rule), "{query:?}: {err}");
    }
    // The POST endpoints and the other shapes never look at the query.
    for path in [
        "/acme/x.git/git-upload-pack",
        "/acme/x.git/git-receive-pack",
        "/acme/x",
    ] {
        assert!(
            parse_web_path(path, Some("service=bogus&&")).is_ok(),
            "{path}"
        );
    }
}

#[test]
fn git_route_and_instance_route_names() {
    assert_eq!(GitRoute::InfoRefsUploadPack.service(), "git-upload-pack");
    assert_eq!(GitRoute::UploadPack.service(), "git-upload-pack");
    assert_eq!(GitRoute::InfoRefsReceivePack.service(), "git-receive-pack");
    assert_eq!(GitRoute::ReceivePack.service(), "git-receive-pack");
    let ads: Vec<_> = GitRoute::ALL
        .iter()
        .filter(|r| r.is_advertisement())
        .collect();
    assert_eq!(
        ads,
        [
            &GitRoute::InfoRefsUploadPack,
            &GitRoute::InfoRefsReceivePack
        ]
    );
    let pushes: Vec<_> = GitRoute::ALL.iter().filter(|r| r.is_push()).collect();
    assert_eq!(
        pushes,
        [&GitRoute::InfoRefsReceivePack, &GitRoute::ReceivePack]
    );
    let names: BTreeSet<_> = GitRoute::ALL.iter().map(|r| r.as_str()).collect();
    assert_eq!(names.len(), GitRoute::ALL.len());
    let words: BTreeSet<_> = RouteWord::ALL.iter().map(|w| w.as_str()).collect();
    assert_eq!(words.len(), RouteWord::ALL.len());
    assert_eq!(
        InstanceRoute::ProjectPermalink.as_str(),
        "project_permalink"
    );
}

#[test]
fn project_permalink_needs_a_positive_id() {
    let parsed = parse_web_path("/-/projects/7/-/tree/main", None).expect("permalink");
    assert_eq!(
        parsed,
        ParsedPath::Instance {
            route: InstanceRoute::ProjectPermalink,
            rest: TailSegments::decode(&["7", "-", "tree", "main"]).expect("tail"),
        }
    );
    assert_eq!(parsed.shape(), "instance");
    assert_eq!(parsed.lookup_path(), None);
    for (path, rule) in [
        ("/-/projects", "missing_id"),
        ("/-/projects/0", "invalid_id"),
        ("/-/projects/00", "invalid_id"),
        ("/-/projects/01", "invalid_id"),
        ("/-/projects/+1", "invalid_id"),
        ("/-/projects/-1", "invalid_id"),
        ("/-/projects/1e3", "invalid_id"),
        ("/-/projects/%31", "invalid_id"),
        ("/-/projects/9223372036854775808", "invalid_id"),
        ("/-/projects/99999999999999999999999", "invalid_id"),
    ] {
        let err = assert_reason(path, None, "path_rejected");
        assert_eq!(ctx(&err, "rule"), Some(rule), "{path}: {err}");
    }
    for path in ["/-", "/-/Projects/1", "/-/static/app.js", "/-/blob/main"] {
        assert_reason(path, None, "unknown_route");
    }
}

/// The id rule of the permalink is the API's too (`/api/v1/projects/{id}`).
#[test]
fn positive_ids_are_plain_decimal_without_a_leading_zero() {
    assert_eq!(crate::parse_positive_id("1"), Some(1));
    assert_eq!(
        crate::parse_positive_id("9223372036854775807"),
        Some(i64::MAX)
    );
    for raw in [
        "",
        "0",
        "00",
        "01",
        "+1",
        "-1",
        "1e3",
        "%31",
        " 1",
        "1 ",
        "9223372036854775808",
        "\u{0661}",
    ] {
        assert_eq!(crate::parse_positive_id(raw), None, "{raw:?}");
    }
}

#[test]
fn shapes_and_lookup_paths() {
    let home = parse_web_path("/acme/x", None).expect("home");
    assert_eq!(home.shape(), "home");
    let git = parse_web_path("/acme/x.git/git-receive-pack", None).expect("git");
    assert_eq!(git.shape(), "git");
    let scoped = parse_web_path("/acme/x/-/tree", None).expect("scoped");
    assert_eq!(scoped.shape(), "scoped");
    for parsed in [&home, &git, &scoped] {
        assert_eq!(
            parsed.lookup_path().map(LookupPath::full_path),
            Some("acme/x".to_owned())
        );
    }
}

#[test]
fn an_overlong_query_is_refused_for_every_shape() {
    let long = format!("service=git-upload-pack&{}", "a".repeat(MAX_QUERY_LEN));
    for path in [
        "/acme/x.git/info/refs",
        "/acme/x.git/git-upload-pack",
        "/acme/x",
        "/-/projects/1",
    ] {
        let err = assert_reason(path, Some(&long), "path_rejected");
        assert_eq!(ctx(&err, "part"), Some("query"), "{path}");
        assert_eq!(ctx(&err, "rule"), Some("too_long"), "{path}");
    }
    let fits = format!("service=git-upload-pack&{}", "a".repeat(MAX_QUERY_LEN - 24));
    assert_eq!(fits.len(), MAX_QUERY_LEN);
    assert!(parse_web_path("/acme/x.git/info/refs", Some(&fits)).is_ok());
}

/// Tail segments may start with `-` (file names do); git argv safety is the
/// caller's (`--` / `--end-of-options`).
#[test]
fn a_tail_segment_may_start_with_a_dash() {
    let parsed = parse_web_path("/acme/x/-/blob/--output=x/-f", None).expect("dash");
    assert_eq!(describe(&parsed)["rest"], json!(["--output=x", "-f"]));
}

#[test]
fn an_overlong_raw_path_is_refused_before_parsing() {
    let path = format!("/acme/x/-/blob/main/{}", "a".repeat(9000));
    let err = assert_reason(&path, None, "path_rejected");
    assert_eq!(ctx(&err, "rule"), Some("too_long"));
    let fits = format!("/acme/x/-/blob/main/{}", "a".repeat(8000));
    assert!(parse_web_path(&fits, None).is_ok());
}

#[test]
fn log_safe_path_cuts_and_escapes() {
    assert_eq!(
        log_safe_path("/acme/x/-/blob/main/日本.md"),
        "/acme/x/-/blob/main/日本.md"
    );
    assert_eq!(
        log_safe_path("/a\u{0}b\nc\u{7f}\u{85}"),
        "/a\\u{0}b\\u{a}c\\u{7f}\\u{85}"
    );
    assert_eq!(log_safe_path("/a\\b"), "/a\\\\b");
    // Bidi controls and zero-width characters cannot disguise a log line.
    assert_eq!(
        log_safe_path("/a\u{202e}b\u{200b}c\u{2066}\u{feff}"),
        "/a\\u{202e}b\\u{200b}c\\u{2066}\\u{feff}"
    );
    let long = format!("/{}", "x".repeat(400));
    let cut = log_safe_path(&long);
    let kept: String = long.chars().take(LOG_PATH_MAX_CHARS).collect();
    assert_eq!(cut, format!("{kept}…(+145)"));
    // Exactly at the limit nothing is cut.
    let edge = "y".repeat(LOG_PATH_MAX_CHARS);
    assert_eq!(log_safe_path(&edge), edge);
    // Escaping never lets a control character through, even when cut.
    let noisy = "\u{1b}[31m".repeat(200);
    let safe = log_safe_path(&noisy);
    assert!(!safe.chars().any(char::is_control), "{safe:?}");
}

#[test]
fn namespace_kinds_and_owners() {
    for kind in NamespaceKind::ALL {
        assert_eq!(NamespaceKind::parse(kind.as_str()), Some(*kind));
    }
    assert_eq!(NamespaceKind::parse("User"), None);
    let access = ProjectAccess {
        visibility: Visibility::Private,
        role: Some(Role::Guest),
        pending_deletion: false,
    };
    let targets = [
        (
            ResolvedTarget::User(UserId::new(1)),
            NamespaceOwner::User(UserId::new(1)),
            NamespaceKind::User,
        ),
        (
            ResolvedTarget::Group {
                id: GroupId::new(2),
                viewer_role: None,
            },
            NamespaceOwner::Group(GroupId::new(2)),
            NamespaceKind::Group,
        ),
        (
            ResolvedTarget::Project {
                id: ProjectId::new(3),
                access,
            },
            NamespaceOwner::Project(ProjectId::new(3)),
            NamespaceKind::Project,
        ),
    ];
    for (i, (target, owner, kind)) in targets.into_iter().enumerate() {
        let resolved = Resolved {
            namespace_id: crate::NamespaceId::new(10),
            target,
        };
        assert_eq!(resolved.owner(), owner);
        assert_eq!(resolved.kind(), kind);
        assert_eq!(owner.raw_id(), i as i64 + 1);
    }
}

/// A valid namespace segment (lowercase, so it survives folding unchanged).
fn segment() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9]{0,8}".prop_filter("not reserved or a device name", |s| {
        crate::PathSegment::parse_top_level(s).is_ok()
    })
}

/// A tail segment of unreserved characters (decodes to itself).
fn tail_segment() -> impl Strategy<Value = String> {
    "[A-Za-z0-9_~-][A-Za-z0-9._~-]{0,10}"
        .prop_filter("not a dot segment", |s| s != "." && s != "..")
}

fn encoded_separator() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec!["%2F", "%2f", "%5C", "%5c"])
}

/// Rejection vectors (tail fragments) that stay rejected wherever they sit.
fn rejected_fragment() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
        "%2F",
        "%2f",
        "%5C",
        "%5c",
        "%00",
        "%0A",
        "%7F",
        "%C2%85",
        "%FF",
        "%E6%97",
        "%zz",
        "%",
        "\\",
        "%E2%80%8B",
        "%E2%80%AE",
        "%E2%81%A6",
        "%EF%BB%BF",
        "%F3%A0%80%81",
        "\u{202e}",
    ])
}

proptest! {
    #[test]
    fn parse_never_panics_and_rejects_encoded_separators(
        any_path in "\\PC{0,120}",
        query in proptest::option::of("\\PC{0,40}"),
        namespace in proptest::collection::vec(segment(), 1..5),
        word in prop::sample::select(RouteWord::ALL.to_vec()),
        tail in proptest::collection::vec(tail_segment(), 1..5),
        at in 0usize..5,
        sep in encoded_separator(),
        fragment in rejected_fragment(),
        prefix in "[a-z]{0,3}",
    ) {
        // Arbitrary input: never a panic; an accepted path is well-formed.
        for raw in [any_path.clone(), format!("/{any_path}"), format!("/acme/x/-/blob/{any_path}")] {
            if let Ok(parsed) = parse_web_path(&raw, query.as_deref()) {
                if let Some(path) = parsed.lookup_path() {
                    prop_assert!(path.depth() <= MAX_DEPTH);
                    prop_assert!(path.full_path().len() <= 255);
                    prop_assert!(path.full_path().bytes().all(|b| b.is_ascii_lowercase()
                        || b.is_ascii_digit() || b"._-/".contains(&b)));
                }
                if let ParsedPath::Scoped { rest, .. } | ParsedPath::Instance { rest, .. } = &parsed {
                    for s in rest.iter() {
                        prop_assert!(!s.is_empty() && s != "." && s != "..");
                        prop_assert!(!s.contains(['/', '\\']));
                        prop_assert!(!s.chars().any(char::is_control));
                        prop_assert!(!s.chars().any(is_invisible));
                    }
                }
            }
        }
        // A well-formed scoped path parses to exactly its parts.
        let ns = namespace.join("/");
        let good = format!("/{ns}/-/{}/{}", word.as_str(), tail.join("/"));
        let parsed = parse_web_path(&good, None).expect("well-formed");
        prop_assert_eq!(describe(&parsed), json!({
            "kind": "scoped", "namespace": ns, "word": word.as_str(), "rest": tail
        }));
        // An encoded separator anywhere in the tail is refused.
        let mut bad = tail.clone();
        let i = at % bad.len();
        bad[i] = format!("{prefix}{sep}{}", bad[i]);
        let err = parse_web_path(&format!("/{ns}/-/{}/{}", word.as_str(), bad.join("/")), None)
            .expect_err("encoded separator");
        prop_assert_eq!(ctx(&err, "reason"), Some("path_rejected"));
        prop_assert_eq!(ctx(&err, "rule"), Some("encoded_separator"));
        // So is every rejection fragment, in the tail and in the namespace.
        let mut bad = tail.clone();
        bad[i] = format!("{prefix}{fragment}");
        let err = parse_web_path(&format!("/{ns}/-/{}/{}", word.as_str(), bad.join("/")), None)
            .expect_err("rejected fragment in the tail");
        prop_assert_eq!(ctx(&err, "reason"), Some("path_rejected"));
        let mut bad_ns = namespace.clone();
        let j = at % bad_ns.len();
        bad_ns[j] = format!("{}{fragment}", bad_ns[j]);
        let err = parse_web_path(&format!("/{}/-/{}", bad_ns.join("/"), word.as_str()), None)
            .expect_err("rejected fragment in the namespace");
        prop_assert_eq!(ctx(&err, "reason"), Some("path_rejected"));
        // And the same namespace as Smart HTTP parses with .git on the last segment only.
        let git = parse_web_path(&format!("/{ns}.git/git-upload-pack"), None).expect("git");
        prop_assert_eq!(git.lookup_path().map(LookupPath::full_path), Some(ns.clone()));
        if namespace.len() > 1 {
            let first_dot_git = format!("/{}.git/{}/git-upload-pack", namespace[0], namespace[1..].join("/"));
            let err = parse_web_path(&first_dot_git, None).expect_err(".git not last");
            prop_assert_eq!(ctx(&err, "reason"), Some("path_rejected"));
        }
    }
}
