#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly
//! A commit addressed by id is browsable only when a branch or a tag
//! reaches it (M3 P6, ADR-0024 §2, closing table #25 / #37) — real git
//! repositories with synthetic content.

use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};

use super::tests::{blob, commit, fixture, mktree, plumb, reason, update_ref};
use super::*;

/// A linear history `root → mid → tip` on `main` (tip is HEAD), plus:
/// - `gone`: a child of `tip` whose only branch was deleted;
/// - `forced`: the old tip of `topic`, force-moved back to `root`;
/// - `dangling`: made by `commit-tree` only, never on a ref;
/// - `tagged`: only reachable from the annotated tag `v2`, and
///   `light_parent` → `light`: only reachable from the lightweight tag
///   `l1` (its parent is reachable through it too).
struct Graph {
    root: String,
    mid: String,
    tip: String,
    gone: String,
    forced: String,
    dangling: String,
    tagged: String,
    light_parent: String,
    light: String,
}

fn commit_msg(repo: &std::path::Path, tree: &str, parent: Option<&str>, msg: &str) -> String {
    let mut args = vec!["commit-tree", tree, "-m", msg];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

fn graph(repo: &std::path::Path) -> Graph {
    let readme = blob(repo, b"# synthetic\n");
    let tree = mktree(repo, &[("100644", "blob", &readme, "README.md")]);
    let root = commit(repo, &tree, None);
    let mid = commit_msg(repo, &tree, Some(&root), "mid");
    let tip = commit_msg(repo, &tree, Some(&mid), "tip");
    update_ref(repo, "refs/heads/main", &tip);
    plumb(repo, &["symbolic-ref", "HEAD", "refs/heads/main"], None);

    let secret = blob(repo, b"synthetic-secret-value\n");
    let secret_tree = mktree(
        repo,
        &[
            ("100644", "blob", &readme, "README.md"),
            ("100644", "blob", &secret, "secret.txt"),
        ],
    );
    let gone = commit_msg(repo, &secret_tree, Some(&tip), "gone");
    update_ref(repo, "refs/heads/leak", &gone);
    plumb(repo, &["update-ref", "-d", "refs/heads/leak"], None);

    let forced = commit_msg(repo, &secret_tree, Some(&mid), "forced");
    update_ref(repo, "refs/heads/topic", &forced);
    update_ref(repo, "refs/heads/topic", &root);

    let dangling = commit_msg(repo, &secret_tree, Some(&tip), "dangling");

    let tagged = commit_msg(repo, &tree, Some(&root), "tagged");
    let tag = plumb(
        repo,
        &["mktag"],
        Some(
            format!(
                "object {tagged}\ntype commit\ntag v2\ntagger forge-test <forge-test@example.invalid> 0 +0000\n\nsynthetic\n"
            )
            .as_bytes(),
        ),
    );
    update_ref(repo, "refs/tags/v2", &tag);
    let light_parent = commit_msg(repo, &tree, Some(&root), "light parent");
    let light = commit_msg(repo, &tree, Some(&light_parent), "light");
    update_ref(repo, "refs/tags/l1", &light);
    Graph {
        root,
        mid,
        tip,
        gone,
        forced,
        dangling,
        tagged,
        light_parent,
        light,
    }
}

fn segs(spec: &str) -> Vec<String> {
    spec.split('/').map(str::to_owned).collect()
}

/// What a caller is told: the code, and the logged reason.
fn told(err: &ForgeError) -> (ErrorCode, String) {
    (err.code(), reason(err))
}

#[tokio::test]
async fn unreachable_commit_is_not_browsable_by_id() {
    let f = fixture().await;
    let g = graph(&f.path);
    let b = f.browser(BrowseLimits::default());
    let missing = "0".repeat(40);
    let unknown = told(
        &b.resolve_ref(f.id, &missing, "user:1")
            .await
            .expect_err("missing"),
    );
    assert_eq!(
        unknown,
        (
            ErrorCode::RepositoryPathNotResolved,
            "ref_not_found".to_owned()
        )
    );
    for (what, id) in [
        ("deleted branch", &g.gone),
        ("force-pushed away", &g.forced),
        ("commit-tree only", &g.dangling),
    ] {
        let e = b.resolve_ref(f.id, id, "user:1").await.expect_err(what);
        assert_eq!(told(&e), unknown, "resolve_ref: {what}");
        let upper = id.to_ascii_uppercase();
        let e = b.resolve_ref(f.id, &upper, "user:1").await.expect_err(what);
        assert_eq!(told(&e), unknown, "resolve_ref upper-case: {what}");
        let e = b
            .resolve_spec(f.id, &segs(&format!("{id}/secret.txt")), "user:1")
            .await
            .expect_err(what);
        assert_eq!(told(&e), unknown, "resolve_spec: {what}");
        // One commit by id (the commit page) and a history page whose
        // cursor names the unreachable commit as its tip.
        let e = b.commit(f.id, id, "user:1").await.expect_err(what);
        assert_eq!(told(&e), unknown, "commit: {what}");
        let cursor = LogCursor::parse(&format!("v1.{id}.{id}.1"), 100).expect("cursor");
        let e = b
            .commits(f.id, id, &RepoPath::root(), 10, Some(&cursor), "user:1")
            .await
            .expect_err(what);
        assert_eq!(told(&e), unknown, "commits by cursor: {what}");
    }
}

#[tokio::test]
async fn commits_reachable_from_a_branch_or_a_tag_are_browsable_by_id() {
    let f = fixture().await;
    let g = graph(&f.path);
    let b = f.browser(BrowseLimits::default());
    for (what, id) in [
        ("branch tip", &g.tip),
        ("branch ancestor", &g.mid),
        ("root", &g.root),
        ("annotated tag only", &g.tagged),
        ("lightweight tag only", &g.light),
        ("ancestor of a tag only", &g.light_parent),
    ] {
        let r = b.resolve_ref(f.id, id, "user:1").await.expect(what);
        assert_eq!((r.target, r.commit.as_str()), (Target::Commit, id.as_str()));
        let s = b
            .resolve_spec(f.id, &segs(&format!("{id}/README.md")), "user:1")
            .await
            .expect(what);
        assert_eq!(s.rev.commit, *id, "{what}");
        assert_eq!(s.path.as_str(), "README.md");
        let detail = b.commit(f.id, id, "user:1").await.expect(what);
        assert_eq!(detail.info.oid, *id, "{what}");
    }
}

#[tokio::test]
async fn branch_and_tag_names_resolve_as_before() {
    let f = fixture().await;
    let g = graph(&f.path);
    let b = f.browser(BrowseLimits::default());
    assert_eq!(
        b.resolve_ref(f.id, "main", "user:1")
            .await
            .expect("main")
            .commit,
        g.tip
    );
    assert_eq!(
        b.resolve_ref(f.id, "refs/heads/topic", "user:1")
            .await
            .expect("topic")
            .commit,
        g.root
    );
    assert_eq!(
        b.resolve_ref(f.id, "v2", "user:1")
            .await
            .expect("v2")
            .commit,
        g.tagged
    );
    let s = b
        .resolve_spec(f.id, &segs("l1/README.md"), "user:1")
        .await
        .expect("l1");
    assert_eq!(s.rev.commit, g.light);
}

/// Whether the error carries `key=value` in its (logged) context.
fn has(err: &ForgeError, key: &str, value: &str) -> bool {
    err.context().iter().any(|(k, v)| *k == key && v == value)
}

/// Names never pay for the check: with a check that cannot finish, every
/// branch and tag name (and a tip id already in the cached ref list)
/// still resolves.
#[tokio::test]
async fn names_and_cached_tips_skip_the_check() {
    let f = fixture().await;
    let g = graph(&f.path);
    let b = f.browser(BrowseLimits {
        reach_timeout: Duration::from_nanos(1),
        ..BrowseLimits::default()
    });
    for name in ["main", "refs/heads/topic", "v2", "l1"] {
        b.resolve_ref(f.id, name, "user:1").await.expect(name);
    }
    // The ref list is cached now: the tips of `main` and `l1` need no git.
    for tip in [&g.tip, &g.light] {
        let r = b
            .resolve_ref(f.id, tip, "user:1")
            .await
            .expect("cached tip");
        assert_eq!(r.target, Target::Commit);
    }
}

/// A check that runs out of time is never "reachable": the check itself
/// is `FG-GIT-0002 check=reachability`, and an id lookup answers the user
/// exactly like a missing id (`ref_not_found`, the hidden code in the
/// log), so the status does not tell an existing commit from a missing
/// one (review of P6, MEDIUM 2). Real git, so this also runs on Windows:
/// a 1 ns limit always expires before git answers.
#[tokio::test]
async fn a_reachability_check_out_of_time_answers_not_found_never_success() {
    let f = fixture().await;
    let g = graph(&f.path);
    let b = f.browser(BrowseLimits {
        reach_timeout: Duration::from_nanos(1),
        ..BrowseLimits::default()
    });
    let missing = told(
        &b.resolve_ref(f.id, &"0".repeat(40), "user:1")
            .await
            .expect_err("missing"),
    );
    for id in [&g.mid, &g.gone] {
        let e = b
            .resolve_ref(f.id, id, "user:1")
            .await
            .expect_err("timeout");
        assert_eq!(told(&e), missing, "{:?}", e.context());
        assert!(has(&e, "reach_error", "FG-GIT-0002"), "{:?}", e.context());
        let e = b.commit(f.id, id, "user:1").await.expect_err("timeout");
        assert_eq!(told(&e), missing);
        let deadline = Deadline::new(Duration::from_secs(5));
        let e = b
            .reachable(f.id, id, &ReachRoots::branches_and_tags(), deadline)
            .await
            .expect_err("the check itself");
        assert_eq!(e.code(), ErrorCode::GitTimeout);
        assert!(has(&e, "check", "reachability"), "{:?}", e.context());
    }
    // A timed-out answer is not cached: with time, the same id resolves.
    let ok = Browser {
        limits: BrowseLimits::default(),
        ..b.clone() // clone: the same caches, other limits
    };
    assert_eq!(
        ok.resolve_ref(f.id, &g.mid, "user:1")
            .await
            .expect("mid")
            .commit,
        g.mid
    );
}

/// No free slot of the repository's browse cap: `FG-GIT-0004`, never
/// "reachable".
#[tokio::test]
async fn a_reachability_check_without_a_slot_is_git_busy_not_success() {
    let f = fixture().await;
    let g = graph(&f.path);
    let b = f.browser(BrowseLimits {
        max_per_repo: 1,
        ..BrowseLimits::default()
    });
    let held = b.claim(f.id).expect("the only slot");
    let deadline = Deadline::new(Duration::from_secs(5));
    let e = b
        .reachable(f.id, &g.mid, &ReachRoots::branches_and_tags(), deadline)
        .await
        .expect_err("busy");
    assert_eq!(e.code(), ErrorCode::GitBusy);
    assert_eq!(reason(&e), "repo_browse");
    assert!(has(&e, "check", "reachability"), "{:?}", e.context());
    drop(held);
    assert!(b
        .reachable(f.id, &g.mid, &ReachRoots::branches_and_tags(), deadline)
        .await
        .expect("slot free"));
}

/// Answers are cached until a push invalidates the repository (a
/// force-push removing a secret ends a cached "reachable" at once).
#[tokio::test]
async fn reachability_answers_are_cached_until_a_push_invalidates_them() {
    let f = fixture().await;
    let g = graph(&f.path);
    let b = f.browser(BrowseLimits::default());
    // `topic2` -> outer -> inner -> mid: `inner` is reachable through
    // `topic2` only, and is not a tip.
    let readme = blob(&f.path, b"x\n");
    let tree = mktree(&f.path, &[("100644", "blob", &readme, "x")]);
    let inner = commit_msg(&f.path, &tree, Some(&g.mid), "inner");
    let outer = commit_msg(&f.path, &tree, Some(&inner), "outer");
    update_ref(&f.path, "refs/heads/topic2", &outer);
    b.resolve_ref(f.id, &inner, "user:1")
        .await
        .expect("reachable");
    // Force-pushed away outside forge: within the TTL the answer stands.
    plumb(&f.path, &["update-ref", "-d", "refs/heads/topic2"], None);
    b.resolve_ref(f.id, &inner, "user:1").await.expect("cached");
    // A push through forge invalidates: refused at once.
    b.invalidate(f.id);
    let e = b
        .resolve_ref(f.id, &inner, "user:1")
        .await
        .expect_err("gone");
    assert_eq!(told(&e).1, "ref_not_found");
    // A cached "unreachable" ends with the next push as well.
    update_ref(&f.path, "refs/heads/topic3", &outer);
    b.resolve_ref(f.id, &inner, "user:1")
        .await
        .expect_err("still cached");
    b.invalidate(f.id);
    b.resolve_ref(f.id, &inner, "user:1")
        .await
        .expect("reachable again");

    // Nothing kept: every call asks git.
    let none = f.browser(BrowseLimits {
        max_cached_reach: 0,
        ..BrowseLimits::default()
    });
    none.resolve_ref(f.id, &inner, "user:1")
        .await
        .expect("asks");
    plumb(&f.path, &["update-ref", "-d", "refs/heads/topic3"], None);
    none.resolve_ref(f.id, &inner, "user:1")
        .await
        .expect_err("asks again");
}

/// Performance (ADR-0024 #37): thousands of refs over a deep history —
/// the worst case, an id no ref reaches, walks everything — answers well
/// within the check's time limit.
#[tokio::test]
async fn the_check_on_many_refs_and_a_deep_history_stays_within_the_limit() {
    const COMMITS: usize = 4_000;
    const BRANCHES: usize = 2_000;
    const TAGS: usize = 500;
    let f = fixture().await;
    let mut stream = String::new();
    for i in 1..=COMMITS {
        stream.push_str(&format!(
            "commit refs/heads/main\nmark :{i}\ncommitter forge-test <forge-test@example.invalid> {i} +0000\ndata 0\n"
        ));
        if i > 1 {
            stream.push_str(&format!("from :{}\n", i - 1));
        }
        stream.push('\n');
    }
    for b in 0..BRANCHES {
        stream.push_str(&format!("reset refs/heads/b{b}\nfrom :{}\n\n", 1 + b * 2));
    }
    for t in 0..TAGS {
        stream.push_str(&format!("reset refs/tags/t{t}\nfrom :{}\n\n", 1 + t * 8));
    }
    plumb(
        &f.path,
        &["fast-import", "--quiet"],
        Some(stream.as_bytes()),
    );
    let root = plumb(&f.path, &["rev-parse", "refs/heads/b0"], None);
    let tip = plumb(&f.path, &["rev-parse", "refs/heads/main"], None);
    let tree = plumb(&f.path, &["rev-parse", "refs/heads/main^{tree}"], None);
    let dangling = commit_msg(&f.path, &tree, Some(&tip), "dangling");
    let b = f.browser(BrowseLimits::default());
    // The checks themselves: a timeout would be an error here, so both
    // answering proves they finished within `reach_timeout` (5 s) — no
    // wall-clock assertion of its own that a slow CI host could trip.
    let deadline = Deadline::new(Duration::from_secs(10));
    let roots = ReachRoots::branches_and_tags();
    let started = std::time::Instant::now();
    assert!(!b
        .reachable(f.id, &dangling, &roots, deadline)
        .await
        .expect("unreachable, within the limit"));
    assert!(b
        .reachable(f.id, &root, &roots, deadline)
        .await
        .expect("deep root, within the limit"));
    eprintln!(
        "two checks over 4 000 commits / 2 500 refs: {:?}",
        started.elapsed()
    );
    let e = b
        .resolve_ref(f.id, &dangling, "user:1")
        .await
        .expect_err("unreachable");
    assert!(
        !has(&e, "reach_error", "FG-GIT-0002"),
        "decided, not timed out"
    );
    assert_eq!(told(&e).1, "ref_not_found");
}

/// A check that hangs is killed at its own limit (fake git; unix only —
/// the 1 ns test above is the Windows equivalent).
#[cfg(unix)]
#[tokio::test]
async fn a_hanging_reachability_check_is_killed_at_its_limit() {
    let f = fixture().await;
    let g = graph(&f.path);
    let tmp = tempfile::tempdir().expect("tmp");
    let bin = super::tests::wrapping_git(
        tmp.path(),
        "[ \"$1\" = rev-list ] && [ \"$2\" = --max-count=1 ]",
        "exec sleep 5",
    );
    let b = f.browser_with_bin(
        &bin,
        BrowseLimits {
            reach_timeout: Duration::from_millis(300),
            ..BrowseLimits::default()
        },
    );
    let started = std::time::Instant::now();
    let e = b
        .resolve_ref(f.id, &g.mid, "user:1")
        .await
        .expect_err("timeout");
    assert_eq!(told(&e).1, "ref_not_found");
    assert!(has(&e, "reach_error", "FG-GIT-0002"), "{:?}", e.context());
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "killed at the limit"
    );
}

/// Review of P6 (HIGH, ADR-0024 #37): every commit-id lookup counts
/// against its caller — a token bucket and at most `reach_per_caller`
/// lookups at once. Over either limit is `FG-GIT-0004`, never success,
/// and it is decided before the id is looked at, so a missing id and an
/// existing one are refused alike. Names do not count; other callers are
/// not affected.
#[tokio::test]
async fn a_caller_over_the_reachability_limit_is_busy_and_others_still_pass() {
    let f = fixture().await;
    let g = graph(&f.path);
    let missing = "0".repeat(40);
    let b = f.browser(BrowseLimits {
        reach_caller_burst: 3,
        reach_caller_refill: Duration::from_secs(3600),
        ..BrowseLimits::default()
    });
    b.resolve_ref(f.id, &g.mid, "user:1").await.expect("1st");
    b.resolve_ref(f.id, &missing, "user:1")
        .await
        .expect_err("2nd: missing");
    b.resolve_ref(f.id, &g.gone, "user:1")
        .await
        .expect_err("3rd: unreachable");
    for id in [&g.mid, &missing, &g.gone] {
        let e = b.resolve_ref(f.id, id, "user:1").await.expect_err("over");
        assert_eq!(
            (e.code(), reason(&e).as_str()),
            (ErrorCode::GitBusy, "caller_reach_rate"),
            "{:?}",
            e.context()
        );
    }
    let e = b
        .commit(f.id, &g.mid, "user:1")
        .await
        .expect_err("commit page too");
    assert_eq!(e.code(), ErrorCode::GitBusy);
    b.resolve_ref(f.id, "main", "user:1")
        .await
        .expect("names do not count");
    b.resolve_ref(f.id, &g.mid, "user:2")
        .await
        .expect("another caller");

    // At most `reach_per_caller` at once: a third waits, then is refused.
    let c = f.browser(BrowseLimits {
        reach_per_caller: 2,
        reach_caller_wait: Duration::from_millis(50),
        ..BrowseLimits::default()
    });
    let deadline = Deadline::new(Duration::from_secs(5));
    let first = c.caller_gate("user:1", deadline).await.expect("1st slot");
    let _second = c.caller_gate("user:1", deadline).await.expect("2nd slot");
    for id in [&g.mid, &missing] {
        let e = c.resolve_ref(f.id, id, "user:1").await.expect_err("busy");
        assert_eq!(
            (e.code(), reason(&e).as_str()),
            (ErrorCode::GitBusy, "caller_reach"),
            "{:?}",
            e.context()
        );
    }
    c.resolve_ref(f.id, &g.mid, "user:2")
        .await
        .expect("another caller");
    drop(first);
    c.resolve_ref(f.id, &g.mid, "user:1")
        .await
        .expect("a slot is free again");
}

/// Review of P6 (MEDIUM 1): an id that names a tree or a blob answers
/// like a missing id (`ref_not_found`); the log keeps the real reason.
#[tokio::test]
async fn an_id_of_a_tree_or_blob_answers_like_a_missing_id() {
    let f = fixture().await;
    let g = graph(&f.path);
    let b = f.browser(BrowseLimits::default());
    let tree = plumb(
        &f.path,
        &["rev-parse", &format!("{}^{{tree}}", g.tip)],
        None,
    );
    let readme = blob(&f.path, b"# synthetic\n");
    let missing = told(
        &b.resolve_ref(f.id, &"0".repeat(40), "user:1")
            .await
            .expect_err("missing"),
    );
    for id in [&tree, &readme] {
        let e = b.resolve_ref(f.id, id, "user:1").await.expect_err("tree");
        assert_eq!(told(&e), missing);
        assert!(has(&e, "object", "not_a_commit"), "{:?}", e.context());
        let e = b
            .resolve_spec(f.id, &segs(&format!("{id}/README.md")), "user:1")
            .await
            .expect_err("spec");
        assert_eq!(told(&e), missing);
        let e = b.commit(f.id, id, "user:1").await.expect_err("commit");
        assert_eq!(told(&e), missing);
    }
}

/// Review of P6 (MEDIUM 3): the cache generation is per repository — a
/// push to one repository leaves another's answers (and ref list) alone.
#[tokio::test]
async fn a_push_to_one_repository_keeps_another_repositorys_answers() {
    let f = fixture().await;
    let g = graph(&f.path);
    let other = RepoId::new(12);
    f.storage
        .init_bare(&f.git, other, uuid::Uuid::new_v4(), None)
        .await
        .expect("second repository");
    let b = f.browser(BrowseLimits::default());
    let readme = blob(&f.path, b"x\n");
    let tree = mktree(&f.path, &[("100644", "blob", &readme, "x")]);
    let inner = commit_msg(&f.path, &tree, Some(&g.mid), "inner");
    let outer = commit_msg(&f.path, &tree, Some(&inner), "outer");
    update_ref(&f.path, "refs/heads/topic2", &outer);
    b.resolve_ref(f.id, &inner, "user:1")
        .await
        .expect("reachable");
    b.refs(f.id).await.expect("ref list cached");
    plumb(&f.path, &["update-ref", "-d", "refs/heads/topic2"], None);
    b.invalidate(other);
    b.resolve_ref(f.id, &inner, "user:1")
        .await
        .expect("still cached: another repository was pushed");
    assert!(b.cached(f.id).is_some(), "the ref list stays too");
    b.invalidate(f.id);
    b.resolve_ref(f.id, &inner, "user:1")
        .await
        .expect_err("this repository was pushed");
}

/// Review of P6 (MEDIUM 4): a full cache drops its oldest answer, never
/// every answer.
#[tokio::test]
async fn a_full_reachability_cache_drops_the_oldest_answer_only() {
    let f = fixture().await;
    let g = graph(&f.path);
    let b = f.browser(BrowseLimits {
        max_cached_reach: 2,
        ..BrowseLimits::default()
    });
    for id in [&g.mid, &g.light_parent, &g.gone] {
        let _ = b.resolve_ref(f.id, id, "user:1").await; // ignore: only the cache is looked at
    }
    let mut kept = b.reach.cached_ids(f.id);
    kept.sort_unstable();
    let mut want = vec![g.light_parent.clone(), g.gone.clone()];
    want.sort_unstable();
    assert_eq!(kept, want);
}
