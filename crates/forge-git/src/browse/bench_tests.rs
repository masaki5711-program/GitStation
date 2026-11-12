#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::print_stdout
)] // a bench may fail loudly
//! Spawn counts and timings of the browse calls on a real repository
//! (ignored: run on demand, alone, so the process-wide spawn counter only
//! sees this test):
//!
//! ```text
//! cargo nextest run -p forge-git --run-ignored only -E 'test(browse_spawn_bench)' --no-capture
//! ```
//!
//! `FORGE_BENCH_SRC`: the repository to copy (default: this workspace's
//! own repository); its `HEAD` becomes `main`.

use std::time::{Duration, Instant};

use super::tests::{fixture, plumb};
use super::*;
use crate::git_spawns;

const ROUNDS: u32 = 20;

async fn measure<F, Fut, T>(label: &str, mut op: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ForgeError>>,
{
    // One warm-up run (process pools, caches, the OS file cache).
    op().await.expect(label);
    let spawns = git_spawns();
    let started = Instant::now();
    for _ in 0..ROUNDS {
        op().await.expect(label);
    }
    let per = started.elapsed() / ROUNDS;
    let spawned = (git_spawns() - spawns) as f64 / f64::from(ROUNDS);
    println!(
        "BENCH {label:<32} {:>8.2} ms  {spawned:>5.2} spawns/op",
        ms(per)
    );
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[tokio::test]
#[ignore = "bench: run on demand (see the module documentation)"]
async fn browse_spawn_bench() {
    let f = fixture().await;
    let src = std::env::var("FORGE_BENCH_SRC")
        .unwrap_or_else(|_| format!("{}/../..", env!("CARGO_MANIFEST_DIR")));
    plumb(
        &f.path,
        &[
            "fetch",
            "--no-tags",
            "--quiet",
            &src,
            "+HEAD:refs/heads/main",
        ],
        None,
    );
    plumb(&f.path, &["symbolic-ref", "HEAD", "refs/heads/main"], None);
    // Before: the one-shot commands only (no batch process, no cache).
    let before = BrowseLimits {
        batch_per_repo: 0,
        object_cache_bytes: 0,
        ..BrowseLimits::default()
    };
    for (label, limits) in [("before", before), ("after", BrowseLimits::default())] {
        println!("BENCH --- {label}");
        run(&f, f.browser(limits)).await;
    }
}

async fn run(f: &super::tests::Fixture, b: Browser) {
    let id = f.id;
    let head = b
        .resolve_ref(id, "main", "user:1")
        .await
        .expect("main")
        .commit;
    let root = RepoPath::root();
    let dir = RepoPath::parse("crates/forge-git/src").expect("dir");
    let file = RepoPath::parse("crates/forge-git/src/lib.rs").expect("file");
    let spec = ["main", "crates", "forge-git", "src"];

    measure("refs (cached)", || b.refs(id)).await;
    measure("refs (cold)", || {
        b.invalidate(id);
        b.refs(id)
    })
    .await;
    measure("resolve_spec main/crates/…", || {
        b.resolve_spec(id, &spec, "user:1")
    })
    .await;
    measure("resolve_ref <commit id>", || {
        b.resolve_ref(id, &head, "user:1")
    })
    .await;
    measure("tree /", || b.tree(id, &head, &root)).await;
    measure("tree crates/forge-git/src", || b.tree(id, &head, &dir)).await;
    measure("blob crates/forge-git/src/lib.rs", || {
        b.blob(id, &head, &file)
    })
    .await;
    measure("commits / (20)", || {
        b.commits(id, &head, &root, 20, None, "user:1")
    })
    .await;
    measure("commits crates/forge-git/src (20)", || {
        b.commits(id, &head, &dir, 20, None, "user:1")
    })
    .await;
    // The routes: `?ref=main` resolved, then the call.
    measure("route tree ?ref=main&path=dir", || async {
        let rev = b.resolve_ref(id, "main", "user:1").await?;
        b.tree(id, &rev.commit, &dir).await
    })
    .await;
    measure("route blob ?ref=main&path=file", || async {
        let rev = b.resolve_ref(id, "main", "user:1").await?;
        b.blob(id, &rev.commit, &file).await
    })
    .await;
    measure("route tree ?ref=<id>&path=dir", || async {
        let rev = b.resolve_ref(id, &head, "user:1").await?;
        b.tree(id, &rev.commit, &dir).await
    })
    .await;
}
