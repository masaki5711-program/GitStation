#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly
//! The history half of the browse API end to end through the product
//! router (see `browse_tests.rs` for the fixture): commits with a cursor,
//! one commit with its diff, compare, blame, the caps and the allow / deny
//! table of every route.

use std::time::Duration;

use axum::http::StatusCode;
use forge_core::{RepositoryState, Role, Visibility};
use forge_errors::ErrorCode;
use forge_git::browse::BrowseLimits;
use forge_git::{Git, GitConfig};
use serde_json::Value;

use crate::routes::browse::tests::{
    blob, plumb, repo, router, send, session, token, Answer, Facts, Repo,
};
use crate::routes::history::cursor_param;
use forge_auth::{Principal, Scope};

const API: &str = "/api/v1/projects/7/repository";

/// Three commits on `main` (the fixture's one is the root, also `v1` and
/// `feature/x`): `c1` changes `README.md`, renames `仕様.md` to `spec.md`
/// (same blob), changes `bin.dat` and adds `notes.txt`; `c2` appends to
/// `notes.txt`.
struct Chain {
    c0: String,
    c1: String,
    c2: String,
}

fn chain(r: &Repo) -> Chain {
    let p = r.path.as_path();
    let listing = plumb(
        p,
        &["-c", "core.quotepath=false", "ls-tree", &r.commit],
        None,
    );
    let mut entries: Vec<(String, String, String, String)> = listing
        .lines()
        .map(|line| {
            let (head, name) = line.split_once('\t').expect("tab");
            let mut f = head.split(' ');
            (
                f.next().expect("mode").to_owned(),
                f.next().expect("type").to_owned(),
                f.next().expect("oid").to_owned(),
                name.to_owned(),
            )
        })
        .collect();
    let set = |entries: &mut Vec<(String, String, String, String)>, name: &str, oid: String| {
        let e = entries.iter_mut().find(|e| e.3 == name).expect(name);
        e.2 = oid;
    };
    set(&mut entries, "README.md", blob(p, b"# synthetic\nsecond\n"));
    set(&mut entries, "bin.dat", blob(p, b"\0\x01\x02changed"));
    let spec = entries.iter_mut().find(|e| e.3 == "仕様.md").expect("spec");
    "spec.md".clone_into(&mut spec.3);
    entries.push((
        "100644".to_owned(),
        "blob".to_owned(),
        blob(p, b"one\n"),
        "notes.txt".to_owned(),
    ));
    let mktree = |entries: &[(String, String, String, String)]| {
        let mut sorted = entries.to_vec();
        sorted.sort_by(|a, b| a.3.cmp(&b.3));
        let text: String = sorted
            .iter()
            .map(|(m, t, o, n)| format!("{m} {t} {o}\t{n}\n"))
            .collect();
        plumb(p, &["mktree"], Some(text.as_bytes()))
    };
    let t1 = mktree(&entries);
    let c1 = plumb(
        p,
        &["commit-tree", &t1, "-p", &r.commit, "-m", "second\n\nbody"],
        None,
    );
    set(&mut entries, "notes.txt", blob(p, b"one\ntwo\n"));
    let t2 = mktree(&entries);
    let c2 = plumb(p, &["commit-tree", &t2, "-p", &c1, "-m", "third"], None);
    plumb(p, &["update-ref", "refs/heads/main", &c2], None);
    Chain {
        c0: r.commit.clone(),
        c1,
        c2,
    }
}

async fn get(r: &Repo, uri: &str) -> Answer {
    send(
        router(r, Facts::default(), BrowseLimits::default()),
        "GET",
        uri,
        Some(session()),
    )
    .await
}

fn oids(v: &Value) -> Vec<String> {
    v.as_array()
        .expect("array")
        .iter()
        .map(|c| c["oid"].as_str().expect("oid").to_owned())
        .collect()
}

#[tokio::test]
async fn commits_page_through_a_cursor_from_the_commit_the_first_page_read() {
    let r = repo().await;
    let c = chain(&r);
    let first = get(&r, &format!("{API}/commits?ref=main&limit=2")).await;
    assert_eq!(first.status, StatusCode::OK, "{:?}", first.json());
    let body = first.json();
    assert_eq!(body["ref"]["name"], "main");
    assert_eq!(body["head"], c.c2);
    assert_eq!(oids(&body["commits"]), vec![c.c2.clone(), c.c1.clone()]);
    assert_eq!(body["commits"][0]["subject"], "third");
    assert_eq!(body["commits"][0]["parents"][0], c.c1);
    assert_eq!(body["has_more"], true);
    assert_eq!(first.header("cache-control"), "private, no-store");
    let cursor = body["next_cursor"].as_str().expect("cursor").to_owned();

    // A push moves main meanwhile: the next page still continues the walk.
    let moved = plumb(
        &r.path,
        &[
            "commit-tree",
            &format!("{}^{{tree}}", c.c2),
            "-p",
            &c.c2,
            "-m",
            "later",
        ],
        None,
    );
    plumb(&r.path, &["update-ref", "refs/heads/main", &moved], None);
    let second = send(
        router(&r, Facts::default(), BrowseLimits::default()),
        "GET",
        &format!("{API}/commits?ref=main&limit=2&cursor={cursor}"),
        Some(session()),
    )
    .await;
    assert_eq!(second.status, StatusCode::OK, "{:?}", second.json());
    let body = second.json();
    assert_eq!(
        body["head"], c.c2,
        "the cursor keeps the first page's commit"
    );
    assert_eq!(oids(&body["commits"]), vec![c.c0.clone()]);
    assert_eq!(body["has_more"], false);
    assert_eq!(body["next_cursor"], Value::Null);

    // A path limits the history; its cursor belongs to that path.
    let by_path = get(
        &r,
        &format!("{API}/commits?ref={}&path=README.md&limit=1", c.c2),
    )
    .await
    .json();
    assert_eq!(oids(&by_path["commits"]), vec![c.c1.clone()]);
    let cursor = by_path["next_cursor"].as_str().expect("cursor");
    let other = get(
        &r,
        &format!(
            "{API}/commits?ref={}&path=notes.txt&limit=1&cursor={cursor}",
            c.c2
        ),
    )
    .await;
    assert_eq!(other.status, StatusCode::BAD_REQUEST);
    assert_eq!(other.code(), ErrorCode::RequestInvalid.id());
}

#[tokio::test]
async fn commits_refuse_bad_parameters_and_unknown_refs() {
    let r = repo().await;
    let c = chain(&r);
    let zero = "0".repeat(40);
    for query in [
        "limit=2",
        "ref=main&limit=0",
        "ref=main&limit=101",
        "ref=main&limit=-1",
        "ref=main&limit=abc",
        "ref=main&limit=1&limit=2",
        "ref=main&cursor=",
        "ref=main&cursor=garbage",
        &format!("ref=main&cursor=v1.{0}.{0}.0", c.c2),
        &format!("ref=main&cursor=v1.{0}.{0}.999999999", c.c2),
        "ref=main&path=a/../b",
        "ref=main&x=1",
        "ref=%zz",
    ] {
        let a = get(&r, &format!("{API}/commits?{query}")).await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "{query}: {:?}", a.json());
        assert_eq!(a.code(), ErrorCode::RequestInvalid.id(), "{query}");
    }
    // A well-formed cursor that does not belong here: refused, not guessed.
    let forged = format!("ref=main&cursor=v1.{}.{}.1", c.c2, c.c0);
    let a = get(&r, &format!("{API}/commits?{forged}")).await;
    assert_eq!(a.code(), ErrorCode::RequestInvalid.id());
    // A cursor naming a commit that does not exist.
    let a = get(
        &r,
        &format!("{API}/commits?ref=main&cursor=v1.{zero}.{zero}.1"),
    )
    .await;
    assert_eq!(a.code(), ErrorCode::RepositoryPathNotResolved.id());
    // Option-like refs and paths are names that do not exist.
    for query in ["ref=--output=x", "ref=-n"] {
        let a = get(&r, &format!("{API}/commits?{query}")).await;
        assert_eq!(
            a.code(),
            ErrorCode::RepositoryPathNotResolved.id(),
            "{query}"
        );
    }
    let a = get(&r, &format!("{API}/commits?ref=main&path=--output=written")).await;
    assert_eq!(a.status, StatusCode::OK);
    assert_eq!(a.json()["commits"], serde_json::json!([]));
    assert!(!r.path.join("written").exists());
}

#[tokio::test]
async fn a_commit_shows_its_message_and_its_diff() {
    let r = repo().await;
    let c = chain(&r);
    let a = get(&r, &format!("{API}/commits/{}", c.c1)).await;
    assert_eq!(a.status, StatusCode::OK, "{:?}", a.json());
    let body = a.json();
    assert_eq!(body["commit"]["oid"], c.c1);
    assert_eq!(body["commit"]["subject"], "second");
    assert_eq!(
        body["message"].as_str().unwrap().trim_end(),
        "second\n\nbody"
    );
    assert_eq!(body["diff"]["truncated"], false);
    let files = body["diff"]["files"].as_array().expect("files");
    let by = |name: &str| {
        files
            .iter()
            .find(|f| f["new_path"] == name)
            .unwrap_or_else(|| panic!("{name} in {files:?}"))
    };
    let readme = by("README.md");
    assert_eq!(readme["status"], "modified");
    assert_eq!(readme["patch"], "@@ -1 +1,2 @@\n # synthetic\n+second\n");
    assert_eq!(readme["omitted"], Value::Null);
    let renamed = by("spec.md");
    assert_eq!(renamed["status"], "renamed");
    assert_eq!(renamed["old_path"], "仕様.md");
    assert_eq!(renamed["similarity"], 100);
    assert_eq!(renamed["patch"], "");
    let bin = by("bin.dat");
    assert_eq!(bin["binary"], true);
    assert_eq!(bin["patch"], Value::Null);
    assert_eq!(by("notes.txt")["status"], "added");

    // The root commit is compared with nothing; ids in any case work.
    let root = get(&r, &format!("{API}/commits/{}", c.c0.to_uppercase())).await;
    assert_eq!(root.status, StatusCode::OK, "{:?}", root.json());
    assert_eq!(root.json()["commit"]["parents"], serde_json::json!([]));
    assert!(root.json()["diff"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .all(|f| f["status"] == "added"));

    // The caps.
    let few = BrowseLimits {
        max_diff_files: 2,
        ..BrowseLimits::default()
    };
    let a = send(
        router(&r, Facts::default(), few),
        "GET",
        &format!("{API}/commits/{}", c.c1),
        Some(session()),
    )
    .await;
    assert_eq!(a.json()["diff"]["files"].as_array().unwrap().len(), 2);
    assert_eq!(a.json()["diff"]["truncated"], true);
    let small = BrowseLimits {
        max_file_patch_bytes: 10,
        ..BrowseLimits::default()
    };
    let a = send(
        router(&r, Facts::default(), small),
        "GET",
        &format!("{API}/commits/{}", c.c1),
        Some(session()),
    )
    .await;
    let readme = a.json()["diff"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["new_path"] == "README.md")
        .cloned()
        .unwrap();
    assert_eq!(readme["omitted"], "too_large");
    assert_eq!(readme["patch"], Value::Null);
    assert_eq!(a.json()["diff"]["truncated"], true);
}

#[tokio::test]
async fn a_commit_needs_a_full_id_that_is_a_commit() {
    let r = repo().await;
    let c = chain(&r);
    let tree = plumb(&r.path, &["rev-parse", &format!("{}^{{tree}}", c.c2)], None);
    for tail in [
        "main".to_owned(),
        "HEAD".to_owned(),
        c.c1[..39].to_owned(),
        "--output=x".to_owned(),
        format!("{}?x=1", c.c1),
    ] {
        let a = get(&r, &format!("{API}/commits/{tail}")).await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "{tail}: {:?}", a.json());
        assert_eq!(a.code(), ErrorCode::RequestInvalid.id(), "{tail}");
    }
    for id in ["0".repeat(40), tree] {
        let a = get(&r, &format!("{API}/commits/{id}")).await;
        assert_eq!(a.status, StatusCode::NOT_FOUND, "{id}");
        assert_eq!(a.code(), ErrorCode::RepositoryPathNotResolved.id(), "{id}");
    }
}

#[tokio::test]
async fn compare_lists_commits_and_the_tree_diff() {
    let r = repo().await;
    let c = chain(&r);
    let a = get(&r, &format!("{API}/compare?from=v1&to=main")).await;
    assert_eq!(a.status, StatusCode::OK, "{:?}", a.json());
    let body = a.json();
    assert_eq!(body["from"]["type"], "tag");
    assert_eq!(body["to"]["commit"], c.c2);
    assert_eq!(oids(&body["commits"]), vec![c.c2.clone(), c.c1.clone()]);
    assert_eq!(body["commits_truncated"], false);
    let files = body["diff"]["files"].as_array().unwrap();
    assert!(files.iter().any(|f| f["new_path"] == "notes.txt"));
    assert_eq!(body["diff"]["truncated"], false);

    let back = get(&r, &format!("{API}/compare?from=main&to=v1"))
        .await
        .json();
    assert_eq!(back["commits"], serde_json::json!([]));
    assert!(back["diff"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["old_path"] == "notes.txt" && f["status"] == "deleted"));

    let capped = BrowseLimits {
        max_compare_commits: 1,
        ..BrowseLimits::default()
    };
    let a = send(
        router(&r, Facts::default(), capped),
        "GET",
        &format!("{API}/compare?from=v1&to=main"),
        Some(session()),
    )
    .await;
    assert_eq!(a.json()["commits"].as_array().unwrap().len(), 1);
    assert_eq!(a.json()["commits_truncated"], true);

    for query in [
        "from=v1",
        "to=main",
        "from=&to=main",
        "from=v1&to=main&x=1",
        "from=v1&from=v1&to=main",
    ] {
        let a = get(&r, &format!("{API}/compare?{query}")).await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "{query}");
    }
    for query in [
        "from=nope&to=main",
        "from=--output=x&to=main",
        "from=v1&to=-n",
    ] {
        let a = get(&r, &format!("{API}/compare?{query}")).await;
        assert_eq!(
            a.code(),
            ErrorCode::RepositoryPathNotResolved.id(),
            "{query}"
        );
    }
}

#[tokio::test]
async fn blame_returns_hunks_and_the_commits_they_name() {
    let r = repo().await;
    let c = chain(&r);
    let a = get(&r, &format!("{API}/blame?ref=main&path=notes.txt")).await;
    assert_eq!(a.status, StatusCode::OK, "{:?}", a.json());
    let body = a.json();
    assert_eq!(body["binary"], false);
    assert_eq!(body["truncated"], false);
    assert_eq!(body["size"], 8);
    let hunks = body["hunks"].as_array().unwrap();
    assert_eq!(hunks.len(), 2);
    assert_eq!(hunks[0]["commit"], c.c1);
    assert_eq!(hunks[0]["lines"], serde_json::json!(["one"]));
    assert_eq!(hunks[1]["commit"], c.c2);
    assert_eq!(
        (hunks[1]["start"].as_u64(), hunks[1]["orig_start"].as_u64()),
        (Some(2), Some(2))
    );
    let commits = body["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 2);
    assert_eq!(commits[0]["summary"], "second");
    assert_eq!(commits[0]["author_email"], "forge-test@example.invalid");
    assert!(commits[0]["author_time"].as_i64().unwrap() > 0);

    let a = get(&r, &format!("{API}/blame?ref=main&path=bin.dat")).await;
    assert_eq!(a.json()["binary"], true);
    assert_eq!(a.json()["hunks"], serde_json::json!([]));
    let a = get(&r, &format!("{API}/blame?ref=main&path=--output=x")).await;
    assert_eq!(
        a.json()["hunks"][0]["lines"],
        serde_json::json!(["option-like"])
    );
    assert!(!r.path.join("x").exists());

    let capped = BrowseLimits {
        max_blame_lines: 1,
        ..BrowseLimits::default()
    };
    let a = send(
        router(&r, Facts::default(), capped),
        "GET",
        &format!("{API}/blame?ref=main&path=notes.txt"),
        Some(session()),
    )
    .await;
    assert_eq!(a.json()["truncated"], true);

    for query in [
        "ref=main",
        "path=notes.txt",
        "ref=main&path=",
        "ref=main&path=a//b",
        "ref=main&path=x&y=1",
    ] {
        let a = get(&r, &format!("{API}/blame?{query}")).await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "{query}");
    }
    for query in ["ref=main&path=src", "ref=main&path=nope.txt"] {
        let a = get(&r, &format!("{API}/blame?{query}")).await;
        assert_eq!(a.status, StatusCode::NOT_FOUND, "{query}");
        assert_eq!(
            a.code(),
            ErrorCode::RepositoryPathNotResolved.id(),
            "{query}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_blame_past_its_time_limit_is_a_git_timeout() {
    use std::os::unix::fs::PermissionsExt;
    let mut r = repo().await;
    chain(&r);
    let bin = r._root.path().join("slow-git.sh");
    std::fs::write(
        &bin,
        "#!/bin/sh\nif [ \"$1\" = blame ]; then exec sleep 5; fi\nexec git \"$@\"\n",
    )
    .expect("script");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    r.git = Git::new(GitConfig::new(&bin, r._home.path()));
    let limits = BrowseLimits {
        blame_timeout: Duration::from_millis(500),
        ..BrowseLimits::default()
    };
    let started = std::time::Instant::now();
    let a = send(
        router(&r, Facts::default(), limits),
        "GET",
        &format!("{API}/blame?ref=main&path=notes.txt"),
        Some(session()),
    )
    .await;
    assert_eq!(a.code(), ErrorCode::GitTimeout.id(), "{:?}", a.json());
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[tokio::test]
async fn the_history_routes_need_read_api_and_a_reader() {
    let r = repo().await;
    let c = chain(&r);
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
                deleted: true,
                ..Facts::default()
            },
            Some(session()),
            StatusCode::GONE,
            Some(ErrorCode::PendingDeletion),
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
    let routes = [
        "commits?ref=main".to_owned(),
        format!("commits/{}", c.c1),
        "compare?from=v1&to=main".to_owned(),
        "blame?ref=main&path=README.md".to_owned(),
    ];
    for route in &routes {
        for (facts, principal, status, code) in &cases {
            let a = send(
                router(&r, *facts, BrowseLimits::default()),
                "GET",
                &format!("{API}/{route}"),
                principal.clone(), // clone: each route sends its own
            )
            .await;
            if let Some(code) = code {
                assert_eq!(a.code(), code.id(), "{route}: {:?}", a.json());
                assert_eq!(a.status.as_u16(), code.http_status(), "{route} {code:?}");
            } else {
                assert_eq!(a.status, *status, "{route}: {:?}", a.json());
            }
        }
        // Another project id: the same 404 as an invisible one; a leading
        // zero is not an id.
        let a = get(&r, &format!("/api/v1/projects/8/repository/{route}")).await;
        assert_eq!(a.code(), ErrorCode::NotFound.id(), "{route}");
        let a = get(&r, &format!("/api/v1/projects/07/repository/{route}")).await;
        assert_eq!(a.code(), ErrorCode::RequestInvalid.id(), "{route}");
    }
}

#[test]
fn a_cursor_that_is_not_one_is_cursor_invalid_with_the_payload_rule() {
    let rule_of = |e: &forge_errors::ForgeError| {
        let get = |k: &str| {
            e.context()
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        (get("reason"), get("rule"))
    };
    let params = |v: &str| vec![("cursor", v.to_owned())];
    for bad in ["garbage", "v1.a.b.1", "v2.x.y.1"] {
        let e = cursor_param(&params(bad), 100).expect_err("bad cursor");
        assert_eq!(e.code(), ErrorCode::RequestInvalid);
        assert_eq!(rule_of(&e), ("cursor_invalid".into(), "payload".into()));
    }
    assert!(cursor_param(&[], 100).expect("none").is_none());
}
