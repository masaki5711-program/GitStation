#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly
//! The document API `GET /api/v1/projects/{id}/documents[/{doc_id}]` end
//! to end through the product router (plan 13 stage 3): the browse API's
//! scripted project (no database) and a real bare repository holding the
//! synthetic examples of plan 13 (`docs/plan/13-examples/`). Synthetic data
//! only.

use std::path::Path;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal, Scope};
use forge_core::{RepositoryState, Role, UserId, Visibility};
use forge_errors::ErrorCode;
use forge_git::browse::BrowseLimits;
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use super::{value_json, view_parts, DocIndex};
use crate::routes::browse::tests::{
    blob, plumb, repo, router, send, session, token, Answer, Facts, Repo,
};
use forge_checks::doc_lint::DocTree;
use forge_doc::document::{parse_document, TypeDefault, Value as DocValue};
use forge_doc::render::view_with_body;

const QP: &str = include_str!("../../../../docs/plan/13-examples/qms/procedures/QP-07-001.md");
const RA: &str = include_str!("../../../../docs/plan/13-examples/safety/RA-2026-014.md");
const URS: &str = include_str!("../../../../docs/plan/13-examples/equipment/URS-L3-001.md");
const T_QP: &str =
    include_str!("../../../../docs/plan/13-examples/.forge/doctypes/qms.procedure.yml");
const T_RA: &str =
    include_str!("../../../../docs/plan/13-examples/.forge/doctypes/safety.risk-assessment.yml");
const T_URS: &str =
    include_str!("../../../../docs/plan/13-examples/.forge/doctypes/equipment.urs.yml");

const API: &str = "/api/v1/projects/7/documents";
/// The URS's title: never in an answer about a less confidential document.
const URS_TITLE: &str = "設備要求仕様書";

const RA_EN: &str = "---\ndoctype: safety.risk-assessment\nid: RA-2026-014\ntitle: Press No.3 risk assessment\nowner: safety-team\nlang: en\ntranslation_of: RA-2026-014\n---\n\n## Scope\n\nThe press.\nIts guard.\n";

const NO_TYPE: &str =
    "---\ndoctype: no.such-type\nid: NT-1\ntitle: Untyped\nowner: someone\n---\n\n## A\n\nText.\n";

/// A commit of nested `files`.
fn commit(repo: &Path, files: &[(&str, &str)], parent: Option<&str>) -> String {
    use std::collections::BTreeMap;
    #[derive(Default)]
    struct Dir {
        files: BTreeMap<String, String>,
        dirs: BTreeMap<String, Dir>,
    }
    fn write(repo: &Path, d: &Dir) -> String {
        let mut listing = String::new();
        for (name, sub) in &d.dirs {
            listing.push_str(&format!("040000 tree {}\t{name}\n", write(repo, sub)));
        }
        for (name, b) in &d.files {
            listing.push_str(&format!("100644 blob {b}\t{name}\n"));
        }
        plumb(repo, &["mktree"], Some(listing.as_bytes()))
    }
    let mut root = Dir::default();
    for (path, content) in files {
        let b = blob(repo, content.as_bytes());
        let mut segs: Vec<&str> = path.split('/').collect();
        let name = segs.pop().expect("name");
        let mut d = &mut root;
        for s in segs {
            d = d.dirs.entry(s.to_owned()).or_default();
        }
        d.files.insert(name.to_owned(), b);
    }
    let tree = write(repo, &root);
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "synthetic"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

/// The browse fixture's repository with the examples: `main` (the
/// examples, a translation and a document of an unknown type) and tag
/// `v1` (an older commit where the URS is `internal` and titled
/// differently).
async fn fixture() -> Repo {
    let r = repo().await;
    let urs_v1 = URS
        .replace("classification: confidential", "classification: internal")
        .replace("更新 設備要求仕様書", "旧版");
    let v1 = commit(
        &r.path,
        &[
            (".forge/doctypes/qms.procedure.yml", T_QP),
            (".forge/doctypes/safety.risk-assessment.yml", T_RA),
            (".forge/doctypes/equipment.urs.yml", T_URS),
            ("qms/procedures/QP-07-001.md", QP),
            ("safety/RA-2026-014.md", RA),
            ("equipment/URS-L3-001.md", &urs_v1),
        ],
        None,
    );
    let main = commit(
        &r.path,
        &[
            (".forge/doctypes/qms.procedure.yml", T_QP),
            (".forge/doctypes/safety.risk-assessment.yml", T_RA),
            (".forge/doctypes/equipment.urs.yml", T_URS),
            ("qms/procedures/QP-07-001.md", QP),
            ("safety/RA-2026-014.md", RA),
            ("safety/RA-2026-014.en.md", RA_EN),
            ("equipment/URS-L3-001.md", URS),
            ("misc/NT-1.md", NO_TYPE),
            ("README.md", "# not a document\n"),
        ],
        Some(&v1),
    );
    plumb(&r.path, &["update-ref", "refs/tags/v1", &v1], None);
    plumb(&r.path, &["update-ref", "refs/heads/main", &main], None);
    r
}

fn agent() -> Principal {
    token(&[Scope::ReadApi]).as_agent(true)
}

fn service_account() -> Principal {
    Principal::authenticated(
        UserId::new(5),
        false,
        true,
        Credential::Token(vec![Scope::ReadApi]),
        AccountKind::ServiceAccount,
    )
}

async fn get(r: &Repo, uri: &str, who: Principal) -> Answer {
    send(
        router(r, Facts::default(), BrowseLimits::default()),
        "GET",
        uri,
        Some(who),
    )
    .await
}

async fn get_with_header(r: &Repo, uri: &str, who: Principal) -> Answer {
    let mut req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("forge-context-audience", "agent")
        .body(Body::empty())
        .expect("request");
    req.extensions_mut().insert(who);
    let res = router(r, Facts::default(), BrowseLimits::default())
        .oneshot(req)
        .await
        .expect("infallible");
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    Answer {
        status,
        headers,
        bytes,
    }
}

fn ok(a: &Answer) -> Value {
    assert_eq!(a.status, StatusCode::OK, "{:?}", a.json());
    assert_eq!(a.header("cache-control"), "private, no-store");
    a.json()
}

fn text(a: &Answer) -> String {
    String::from_utf8_lossy(&a.bytes).into_owned()
}

fn without_request_id(a: &Answer) -> Value {
    let mut v = a.json();
    if let Some(o) = v.as_object_mut() {
        o.remove("request_id");
    }
    v
}

fn ref_of<'a>(body: &'a Value, doc: &str, block: Option<&str>) -> &'a Value {
    body["refs"]
        .as_array()
        .expect("refs")
        .iter()
        .find(|r| r["doc"] == doc && r["block"].as_str() == block)
        .unwrap_or_else(|| panic!("no ref {doc} {block:?}"))
}

#[tokio::test]
async fn a_document_is_shown_with_its_sections_blocks_and_references() {
    let r = fixture().await;
    let a = get(&r, &format!("{API}/RA-2026-014"), session()).await;
    let body = ok(&a);
    assert_eq!(body["version"], 1);
    assert_eq!(body["ref"]["name"], "main");
    assert_eq!(body["path"], "safety/RA-2026-014.md");
    assert_eq!(body["id"], "RA-2026-014");
    assert_eq!(body["doctype"], "safety.risk-assessment");
    assert_eq!(body["title"], "プレス機 No.3 リスクアセスメント");
    assert_eq!(body["owner"], "safety-team");
    assert_eq!(body["classification"], "internal");
    assert_eq!(body["lang"], Value::Null);
    assert_eq!(body["translations"], serde_json::json!(["en"]));
    assert_eq!(body["incomplete"], Value::Null);
    // Fields the base keys do not cover, as written.
    let fields = body["fields"].as_array().expect("fields");
    let related = fields
        .iter()
        .find(|f| f["key"] == "related")
        .expect("related");
    assert_eq!(related["value"], serde_json::json!(["URS-L3-001"]));
    // Sections at level-2 headings, with text and blocks.
    let all = body["sections"].as_array().expect("sections");
    // The part before the first heading (the synthetic mark's comment).
    assert_eq!(all[0]["heading"], Value::Null);
    let sections = &all[1..];
    assert_eq!(sections[0]["heading"], "対象と範囲");
    let hazards = &sections[1];
    assert_eq!(hazards["heading"], "危険源の特定とリスクの見積り");
    let h01 = hazards["pieces"]
        .as_array()
        .expect("pieces")
        .iter()
        .find(|p| p["type"] == "block" && p["block"]["id"] == "H-01")
        .expect("H-01");
    assert_eq!(h01["block"]["kind"], "hazard");
    assert_eq!(h01["block"]["attrs"][0]["key"], "severity");
    assert_eq!(h01["block"]["attrs"][0]["value"], "4");
    assert_eq!(h01["block"]["deprecated"], false);
    assert_eq!(h01["markdown"], Value::Null);
    // Text: as written, and for display with the Japanese lines joined.
    let first = &sections[0]["pieces"][0];
    assert_eq!(first["type"], "text");
    let written = first["markdown"].as_str().expect("markdown");
    assert!(written.contains("作業である。\n見積りの尺度"), "{written}");
    let shown = first["display"].as_str().expect("display");
    assert!(shown.contains("作業である。見積りの尺度"), "{shown}");
    // The table's rows stay rows.
    assert!(
        shown.contains("| 値 | 重大さ | 起こりやすさ |\n|---|"),
        "{shown}"
    );
    // References: found (title), hidden (id only: Q14), broken.
    let qp = ref_of(&body, "QP-07-001", None);
    assert_eq!(qp["target"], "found");
    assert_eq!(qp["title"], "文書管理規程");
    let urs = ref_of(&body, "URS-L3-001", Some("REQ-L3-012"));
    assert_eq!(urs["target"], "hidden");
    assert_eq!(urs["title"], Value::Null);
    assert_eq!(urs["block_kind"], Value::Null);
    assert!(!text(&a).contains(URS_TITLE), "{}", text(&a));
}

#[tokio::test]
async fn a_reference_resolves_at_the_revision_read() {
    let r = fixture().await;
    // At v1 the URS was internal: the reference shows its title then.
    let body = ok(&get(&r, &format!("{API}/RA-2026-014?ref=v1"), session()).await);
    assert_eq!(body["ref"]["name"], "v1");
    let urs = ref_of(&body, "URS-L3-001", Some("REQ-L3-012"));
    assert_eq!(urs["target"], "found");
    assert_eq!(urs["title"], "プレス機 No.3 旧版");
    assert_eq!(urs["block_kind"], "req");
    assert_eq!(body["translations"], serde_json::json!([]));
    // The more confidential document itself sees its own references.
    let body = ok(&get(&r, &format!("{API}/URS-L3-001"), session()).await);
    assert_eq!(body["classification"], "confidential");
    let ra = ref_of(&body, "RA-2026-014", Some("C-01a"));
    assert_eq!(ra["target"], "found");
    assert_eq!(ra["block_kind"], "control");
    // A reference to a document of no repository: broken.
    let body = ok(&get(&r, &format!("{API}/QP-07-001"), session()).await);
    assert_eq!(ref_of(&body, "QM-001", None)["target"], "broken");
    // A revision that does not resolve: the browse API's answer.
    let a = get(&r, &format!("{API}/RA-2026-014?ref=nope"), session()).await;
    assert_eq!(a.status, StatusCode::NOT_FOUND);
    assert_eq!(a.code(), ErrorCode::RepositoryPathNotResolved.id());
}

#[tokio::test]
async fn a_translation_is_shown_by_lang() {
    let r = fixture().await;
    let body = ok(&get(&r, &format!("{API}/RA-2026-014?lang=en"), session()).await);
    assert_eq!(body["path"], "safety/RA-2026-014.en.md");
    assert_eq!(body["lang"], "en");
    assert_eq!(body["title"], "Press No.3 risk assessment");
    // At least as confidential as its original.
    assert_eq!(body["classification"], "internal");
    let shown = body["sections"][0]["pieces"][0]["display"]
        .as_str()
        .expect("display");
    assert_eq!(shown, "The press. Its guard.");
    let a = get(&r, &format!("{API}/RA-2026-014?lang=id"), session()).await;
    assert_eq!(a.status, StatusCode::NOT_FOUND, "{:?}", a.json());
    assert_eq!(a.code(), ErrorCode::NotFound.id());
}

#[tokio::test]
async fn a_type_that_cannot_be_read_makes_a_document_confidential() {
    let r = fixture().await;
    let body = ok(&get(&r, &format!("{API}/NT-1"), session()).await);
    assert_eq!(body["classification"], "confidential");
    let a = get(&r, &format!("{API}/NT-1"), agent()).await;
    assert_eq!(a.status, StatusCode::NOT_FOUND);
    assert_eq!(a.code(), ErrorCode::NotFound.id());
}

#[tokio::test]
async fn absent_invisible_and_unknown_documents_answer_the_same_404() {
    let r = fixture().await;
    let none = get(&r, &format!("{API}/NO-SUCH-1"), session()).await;
    assert_eq!(none.status, StatusCode::NOT_FOUND);
    assert_eq!(none.code(), ErrorCode::NotFound.id());
    // A Markdown file that is no document is no document.
    let readme = get(&r, &format!("{API}/README"), session()).await;
    let invisible = send(
        router(
            &r,
            Facts {
                role: None,
                ..Facts::default()
            },
            BrowseLimits::default(),
        ),
        "GET",
        &format!("{API}/RA-2026-014"),
        Some(session()),
    )
    .await;
    let absent = get(&r, "/api/v1/projects/8/documents/RA-2026-014", session()).await;
    let lang = get(&r, &format!("{API}/QP-07-001?lang=en"), session()).await;
    for a in [&readme, &invisible, &absent, &lang] {
        assert_eq!(a.status, StatusCode::NOT_FOUND, "{:?}", a.json());
        assert_eq!(without_request_id(a), without_request_id(&none));
    }
    let list = send(
        router(
            &r,
            Facts {
                role: None,
                ..Facts::default()
            },
            BrowseLimits::default(),
        ),
        "GET",
        API,
        Some(session()),
    )
    .await;
    assert_eq!(without_request_id(&list), without_request_id(&none));
}

#[tokio::test]
async fn the_document_api_needs_read_api_and_a_reader() {
    let r = fixture().await;
    let cases: Vec<(Facts, Option<Principal>, StatusCode, Option<ErrorCode>)> = vec![
        (
            Facts::default(),
            Some(token(&[Scope::ReadApi])),
            StatusCode::OK,
            None,
        ),
        (
            Facts::default(),
            Some(token(&[Scope::ReadRepository])),
            StatusCode::FORBIDDEN,
            Some(ErrorCode::InsufficientScope),
        ),
        (
            Facts {
                role: Some(Role::Guest),
                ..Facts::default()
            },
            Some(session()),
            StatusCode::FORBIDDEN,
            Some(ErrorCode::Forbidden),
        ),
        (
            Facts {
                role: None,
                ..Facts::default()
            },
            Some(session()),
            StatusCode::NOT_FOUND,
            Some(ErrorCode::NotFound),
        ),
        (
            Facts {
                role: None,
                visibility: Visibility::Internal,
                ..Facts::default()
            },
            Some(session()),
            StatusCode::OK,
            None,
        ),
        (
            Facts {
                state: RepositoryState::Provisioning,
                ..Facts::default()
            },
            Some(session()),
            StatusCode::SERVICE_UNAVAILABLE,
            Some(ErrorCode::RepositoryNotReady),
        ),
        (
            Facts::default(),
            None,
            StatusCode::UNAUTHORIZED,
            Some(ErrorCode::Unauthenticated),
        ),
    ];
    for uri in [API.to_owned(), format!("{API}/RA-2026-014")] {
        for (facts, who, status, code) in &cases {
            let a = send(
                router(&r, *facts, BrowseLimits::default()),
                "GET",
                &uri,
                who.clone(),
            )
            .await;
            assert_eq!(a.status, *status, "{uri}: {:?}", a.json());
            if let Some(code) = code {
                assert_eq!(a.code(), code.id(), "{uri}");
            }
        }
    }
}

/// An agent is answered the invisible document's 404 for a confidential
/// document (review stage 3 LOW: no existence oracle), at any revision
/// (marked at the tip), through every kind of agent audience.
#[tokio::test]
async fn a_confidential_document_is_withheld_from_an_agent() {
    let r = fixture().await;
    plumb(
        &r.path,
        &["update-ref", "refs/heads/unmarked", &v1_of(&r)],
        None,
    );
    let uri = format!("{API}/URS-L3-001");
    ok(&get(&r, &uri, session()).await);
    ok(&get(&r, &uri, token(&[Scope::ReadApi])).await);
    let absent = without_request_id(&get(&r, &format!("{API}/NOPE-1"), agent()).await);
    let refusals = [
        get(&r, &uri, agent()).await,
        get(&r, &uri, service_account()).await,
        get_with_header(&r, &uri, session()).await,
        // A revision where it was not marked: still marked at the tip.
        get(&r, &format!("{uri}?ref=v1"), agent()).await,
        // A branch where the mark was removed: the same.
        get(&r, &format!("{uri}?ref=unmarked"), agent()).await,
    ];
    for a in &refusals {
        assert_eq!(a.status, StatusCode::NOT_FOUND, "{:?}", a.json());
        assert_eq!(without_request_id(a), absent, "the same as an absent id");
        assert!(!text(a).contains(URS_TITLE), "{}", text(a));
    }
    // An internal document is given; its reference to the URS is the id only.
    let a = get(&r, &format!("{API}/RA-2026-014"), agent()).await;
    let body = ok(&a);
    assert_eq!(
        ref_of(&body, "URS-L3-001", Some("REQ-L3-012"))["target"],
        "hidden"
    );
    assert!(!text(&a).contains(URS_TITLE));
    // The list leaves it out and does not say how many (review stage 3
    // LOW: a count, or even a flag, with `after` / `doctype` would tell
    // whether a confidential id lies in a range).
    let body = ok(&get(&r, API, agent()).await);
    let ids: Vec<&str> = body["documents"]
        .as_array()
        .expect("documents")
        .iter()
        .map(|d| d["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, ["QP-07-001", "RA-2026-014"]);
    assert_eq!(body["withheld"], Value::Null);
    for q in ["?ref=v1", "?after=RA-2026-014", "?doctype=equipment.urs"] {
        let body = ok(&get(&r, &format!("{API}{q}"), agent()).await);
        assert_eq!(body["withheld"], Value::Null, "{q}");
        assert!(!body.to_string().contains("URS-L3-001"), "{q}");
    }
}

/// The tag `v1` of the fixture.
fn v1_of(r: &Repo) -> String {
    plumb(&r.path, &["rev-parse", "refs/tags/v1"], None)
}

/// Review stage 3 MEDIUM: a reference resolves for an agent against the
/// target's classification at the revision **and** at the default tip — a
/// target internal at `v1` but confidential at the tip is `hidden`, its
/// title never given.
#[tokio::test]
async fn a_reference_to_a_document_confidential_at_the_tip_is_hidden_at_any_revision() {
    let r = fixture().await;
    let uri = format!("{API}/RA-2026-014?ref=v1");
    let person = ok(&get(&r, &uri, session()).await);
    let target = ref_of(&person, "URS-L3-001", Some("REQ-L3-012"));
    assert_eq!(target["target"], "found");
    assert!(person.to_string().contains("旧版"));
    let a = get(&r, &uri, agent()).await;
    let body = ok(&a);
    let target = ref_of(&body, "URS-L3-001", Some("REQ-L3-012"));
    assert_eq!(target["target"], "hidden");
    assert_eq!(target["title"], Value::Null);
    assert_eq!(target["block_kind"], Value::Null);
    assert!(!text(&a).contains("旧版"), "{}", text(&a));
}

/// Review stage 3 LOW: an agent's read fails closed when the default
/// branch is not born (no tip to measure against): every document is
/// withheld; a person reads them.
#[tokio::test]
async fn without_a_default_tip_an_agent_is_given_no_document() {
    let r = fixture().await;
    plumb(
        &r.path,
        &["symbolic-ref", "HEAD", "refs/heads/unborn"],
        None,
    );
    let one = format!("{API}/RA-2026-014?ref=main");
    ok(&get(&r, &one, session()).await);
    let refused = get(&r, &one, agent()).await;
    assert_eq!(
        refused.status,
        StatusCode::NOT_FOUND,
        "{:?}",
        refused.json()
    );
    let list = ok(&get(&r, &format!("{API}?ref=main"), agent()).await);
    assert_eq!(list["documents"], Value::Array(Vec::new()));
    let person = ok(&get(&r, &format!("{API}?ref=main"), session()).await);
    assert_eq!(person["documents"].as_array().expect("docs").len(), 4);
}

/// Review stage 3 LOW: the tip is compared by path as well as by id — a
/// document whose file at the tip cannot be read (no id to compare; fail
/// closed) is not served to an agent at an older revision where it read
/// `internal`.
#[tokio::test]
async fn a_file_confidential_at_the_tip_is_withheld_by_path() {
    let r = fixture().await;
    let tip = plumb(&r.path, &["rev-parse", "refs/heads/main"], None);
    let readable = "---\ndoctype: qms.procedure\nid: QP-09-001\ntitle: Old\nowner: x\nclassification: internal\n---\n\n## A\n\nText.\n";
    let broken = "---\ndoctype: qms.procedure\nid: [QP-09-001\n---\n\n## A\n\nText.\n";
    let old = commit(
        &r.path,
        &[
            (".forge/doctypes/qms.procedure.yml", T_QP),
            ("qms/QP-09-001.md", readable),
        ],
        Some(&tip),
    );
    let new = commit(
        &r.path,
        &[
            (".forge/doctypes/qms.procedure.yml", T_QP),
            ("qms/QP-09-001.md", broken),
        ],
        Some(&old),
    );
    plumb(&r.path, &["update-ref", "refs/heads/old", &old], None);
    plumb(&r.path, &["update-ref", "refs/heads/main", &new], None);
    let uri = format!("{API}/QP-09-001?ref=old");
    ok(&get(&r, &uri, session()).await);
    let a = get(&r, &uri, agent()).await;
    assert_eq!(a.status, StatusCode::NOT_FOUND, "{:?}", a.json());
    let list = ok(&get(&r, &format!("{API}?ref=old"), agent()).await);
    assert!(!list.to_string().contains("QP-09-001"));
}

/// Review stage 3 LOW: for an agent, a translation that is not there and a
/// confidential document answer alike (the confidentiality is decided
/// before the translation is looked up).
#[tokio::test]
async fn an_agent_cannot_tell_a_missing_translation_from_a_confidential_document() {
    let r = fixture().await;
    let absent = without_request_id(&get(&r, &format!("{API}/NOPE-1"), agent()).await);
    for uri in [
        format!("{API}/URS-L3-001?lang=en"),
        format!("{API}/URS-L3-001?lang=id"),
        format!("{API}/RA-2026-014?lang=id"),
    ] {
        let a = get(&r, &uri, agent()).await;
        assert_eq!(a.status, StatusCode::NOT_FOUND, "{uri}: {:?}", a.json());
        assert_eq!(without_request_id(&a), absent, "{uri}");
    }
    ok(&get(&r, &format!("{API}/RA-2026-014?lang=en"), agent()).await);
}

#[tokio::test]
async fn the_list_is_by_id_paged_and_filtered_by_type() {
    let r = fixture().await;
    let body = ok(&get(&r, API, session()).await);
    assert_eq!(body["ref"]["name"], "main");
    assert_eq!(body["withheld"], Value::Null);
    assert_eq!(body["incomplete"], Value::Null);
    assert_eq!(body["next_after"], Value::Null);
    let docs = body["documents"].as_array().expect("documents");
    let ids: Vec<&str> = docs.iter().map(|d| d["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["NT-1", "QP-07-001", "RA-2026-014", "URS-L3-001"]);
    let ra = &docs[2];
    assert_eq!(ra["title"], "プレス機 No.3 リスクアセスメント");
    assert_eq!(ra["doctype"], "safety.risk-assessment");
    assert_eq!(ra["path"], "safety/RA-2026-014.md");
    assert_eq!(ra["classification"], "internal");
    assert_eq!(ra["translations"], serde_json::json!(["en"]));
    assert_eq!(docs[3]["classification"], "confidential");

    let page = ok(&get(&r, &format!("{API}?limit=2"), session()).await);
    assert_eq!(page["documents"].as_array().unwrap().len(), 2);
    assert_eq!(page["next_after"], "QP-07-001");
    let page = ok(&get(&r, &format!("{API}?limit=2&after=QP-07-001"), session()).await);
    let ids: Vec<&str> = page["documents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["RA-2026-014", "URS-L3-001"]);
    assert_eq!(page["next_after"], Value::Null);

    let typed = ok(&get(&r, &format!("{API}?doctype=equipment.urs"), session()).await);
    assert_eq!(typed["documents"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn bad_parameters_are_refused_before_git_runs() {
    let r = fixture().await;
    for uri in [
        format!("{API}/RA%2F1"),
        format!("{API}/-x"),
        format!("{API}/RA-2026-014%23H-01"),
        format!("{API}/{}", "A".repeat(129)),
        format!("{API}/RA-2026-014?lang=EN"),
        format!("{API}/RA-2026-014?nope=1"),
        format!("{API}/RA-2026-014?ref=main&ref=v1"),
        format!("{API}?limit=0"),
        format!("{API}?limit=501"),
        format!("{API}?q=x"),
    ] {
        let a = get(&r, &uri, session()).await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "{uri}: {:?}", a.json());
        assert_eq!(a.code(), ErrorCode::RequestInvalid.id(), "{uri}");
    }
}

// ── the index and the mapping (no git) ─────────────────────────────────

fn tree(files: &[(&str, &str)]) -> DocTree {
    let mut t = DocTree::default();
    for (path, content) in files {
        t.add(path, content.as_bytes().to_vec());
    }
    t.sort();
    t
}

#[test]
fn the_index_keeps_the_first_file_of_an_id_and_its_translations() {
    let other = RA.replace("classification: internal", "classification: public");
    let i = DocIndex::build(tree(&[
        (".forge/doctypes/safety.risk-assessment.yml", T_RA),
        ("a/RA-2026-014.md", RA),
        ("b/RA-2026-014.md", &other),
        ("b/RA-2026-014.en.md", RA_EN),
        ("c/WRONG-NAME.md", RA),
        ("d/plain.md", "# plain\n"),
    ]));
    assert_eq!(i.originals.len(), 1);
    let e = i.original("RA-2026-014").expect("original");
    assert_eq!(e.path, "a/RA-2026-014.md");
    assert_eq!(
        e.ty,
        TypeDefault::Known(Some(forge_doc::document::Classification::Internal))
    );
    assert_eq!(i.languages("RA-2026-014"), ["en"]);
    assert!(i.translation("RA-2026-014", "en").is_some());
    assert!(i.incomplete().is_none());
}

#[test]
fn a_bound_hit_is_named_in_the_answer_and_the_not_found_log() {
    let mut t = tree(&[("a/RA-2026-014.md", RA)]);
    t.left_out("doc_files");
    let i = DocIndex::build(t);
    let inc = i.incomplete().expect("incomplete");
    assert_eq!(inc.error_code, ErrorCode::DocIndexBounded.id());
    assert_eq!(inc.reason, "doc_files");
    let err = super::not_found("document_not_found", &i);
    assert_eq!(err.code(), ErrorCode::NotFound);
    let ctx = format!("{err:?}");
    assert!(ctx.contains("doc_files"), "{ctx}");
    // No type definition was read: the document fails closed.
    let e = i.original("RA-2026-014").expect("original");
    assert_eq!(e.ty, TypeDefault::Unresolved);
}

#[test]
fn the_view_maps_to_the_answer_shape() {
    assert_eq!(value_json(&DocValue::Null), Value::Null);
    let map = DocValue::Map(vec![
        ("rev".into(), 1, DocValue::Text("r3".into())),
        (
            "list".into(),
            2,
            DocValue::List(vec![(2, DocValue::Text("a".into()))]),
        ),
    ]);
    assert_eq!(
        value_json(&map),
        serde_json::json!({"rev": "r3", "list": ["a"]})
    );
    let i = DocIndex::build(tree(&[
        (".forge/doctypes/qms.procedure.yml", T_QP),
        ("qms/procedures/QP-07-001.md", QP),
    ]));
    let doc = parse_document(QP.as_bytes());
    let view = view_with_body(QP, &doc, TypeDefault::Known(None), &i);
    let (fields, sections, refs) = view_parts(&view);
    assert_eq!(fields.len(), view.fields.len());
    assert_eq!(sections.len(), view.sections.len());
    assert_eq!(refs.len(), view.refs.len());
    let deprecated = sections
        .iter()
        .flat_map(|s| &s.pieces)
        .filter_map(|p| p.block.as_ref())
        .find(|b| b.deprecated)
        .expect("a deprecated block in QP-07-001");
    assert!(!deprecated.display.is_empty());
    for p in sections.iter().flat_map(|s| &s.pieces) {
        match p.kind.as_str() {
            "text" => assert!(p.markdown.is_some() && p.display.is_some() && p.block.is_none()),
            "block" => assert!(p.markdown.is_none() && p.display.is_none() && p.block.is_some()),
            other => panic!("{other}"),
        }
    }
    assert!(refs.iter().all(|r| matches!(
        r.target.as_str(),
        "found" | "hidden" | "broken" | "external"
    )));
}
