#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Merge request discussions against a real bare repository and the
//! development database (M3 R3, ADR-0024 §12): an inline position is
//! checked against its version's diff (FG-REVIEW-0004), a later version
//! that rewrites the line marks the thread `outdated`, and the people who
//! may comment and resolve — decided twice, the second time on the facts
//! under the locks.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use common::*;
use forge_auth::{
    prove, AccountKind, Action, Authorized, Credential, DiscussionCtx, LoadedDiscussion,
    MergeRequestCtx, MergeRequestEdit, Principal,
};
use forge_core::{
    DiscussionId, DiscussionSide, ObjectId, ProjectId, RepoId, Role, Scope, UserId, UserKind,
};
use forge_db::repositories;
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_review::{
    DiscussionPosition, NewFields, ResolveOutcome, Review, StoredDiscussion, StoredMergeRequest,
};

fn plumb(repo: &Path, args: &[&str], input: Option<&[u8]>) -> String {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "forge-test")
        .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "forge-test")
        .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("git");
    let mut stdin = child.stdin.take().expect("stdin");
    if let Some(input) = input {
        stdin.write_all(input).expect("write");
    }
    drop(stdin);
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

/// A commit whose only file `lib.rs` has lines `1`..=`20`, with the lines
/// in `changed` replaced.
fn commit(repo: &Path, changed: &[(usize, &str)], parent: Option<&str>) -> String {
    let content: String = (1..=20)
        .map(|n| {
            changed
                .iter()
                .find(|(at, _)| *at == n)
                .map_or_else(|| format!("line {n}\n"), |(_, text)| format!("{text}\n"))
        })
        .collect();
    let blob = plumb(
        repo,
        &["hash-object", "-w", "--stdin"],
        Some(content.as_bytes()),
    );
    let tree = plumb(
        repo,
        &["mktree"],
        Some(format!("100644 blob {blob}\tlib.rs\n").as_bytes()),
    );
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "synthetic"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

fn oid(raw: &str) -> ObjectId {
    ObjectId::parse(raw).expect("oid")
}

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    path: PathBuf,
    review: Review,
    project: ProjectId,
    repo: RepoId,
    base: String,
    v1: String,
    mr: StoredMergeRequest,
    /// The merge request's author (Developer).
    alice: UserId,
    /// Developer, not the author.
    bob: UserId,
    /// Reporter.
    rita: UserId,
    /// A service account with the Developer role.
    svc: UserId,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let owner = user(&t, "olga").await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let rita = user(&t, "rita").await;
    let svc = users::create(
        &t.db,
        &system("req-user-svc"),
        &NewUser {
            kind: UserKind::ServiceAccount,
            email: None,
            ..NewUser::human("ci-bot", "CI", "unused@example.test")
        },
    )
    .await
    .expect("service account")
    .user_id;
    let web = project(
        &t,
        owner,
        &[
            (alice, Role::Developer),
            (bob, Role::Developer),
            (rita, Role::Reporter),
            (svc, Role::Developer),
        ],
    )
    .await;
    let _marked = repositories::mark_ready(&t.db, &system("req-ready"), web.repository_id)
        .await
        .expect("ready");
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let path = storage
        .init_bare(&git, web.repository_id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    let base = commit(&path, &[], None);
    let v1 = commit(&path, &[(10, "ten v1")], Some(&base));
    plumb(&path, &["update-ref", "refs/heads/main", &base], None);
    plumb(&path, &["update-ref", "refs/heads/feature", &v1], None);
    let browser = Arc::new(Browser::new(git, storage, BrowseLimits::default()));
    let review = Review::new(t.db.clone(), browser.clone()).with_diffs(browser);
    let proof = open_proof(
        &t,
        &session(alice),
        web.project_id,
        ("feature", "main"),
        false,
    )
    .await
    .expect("open proof");
    let mr = review
        .open(
            &request("req-open"),
            &proof,
            ready(web.repository_id),
            &NewFields {
                title: "Feature".to_owned(),
                description: String::new(),
            },
        )
        .await
        .expect("open");
    let w = World {
        t,
        _home: home,
        _root: root,
        path,
        review,
        project: web.project_id,
        repo: web.repository_id,
        base,
        v1,
        mr,
        alice,
        bob,
        rita,
        svc,
    };
    w.version(1, &w.v1.clone()).await;
    w
}

impl World {
    /// Record version `n` at `head` as the version job does: the row, its
    /// bases (the base is `main`) and its hidden ref.
    async fn version(&self, n: i64, head: &str) {
        let request = format!("req-v{n}");
        forge_db::review::record_mr_version(
            &self.t.db,
            &system(&request),
            self.mr.id,
            &oid(head),
            None,
        )
        .await
        .expect("version");
        forge_db::review::record_version_bases(
            &self.t.db,
            &system(&format!("{request}-bases")),
            self.mr.id,
            n,
            &oid(&self.base),
            Some(&oid(&self.base)),
        )
        .await
        .expect("bases");
        let iid = self.mr.iid;
        plumb(
            &self.path,
            &[
                "update-ref",
                &format!("refs/merge-requests/{iid}/v{n}"),
                head,
            ],
            None,
        );
        plumb(
            &self.path,
            &[
                "update-ref",
                &format!("refs/merge-requests/{iid}/head"),
                head,
            ],
            None,
        );
    }

    async fn comment_proof(
        &self,
        who: &Principal,
    ) -> Result<Authorized<MergeRequestCtx>, ForgeError> {
        stored_proof(
            &self.t,
            &self.review,
            who,
            Action::MergeRequestComment,
            self.project,
            self.mr.iid,
            MergeRequestEdit::default(),
        )
        .await
    }

    async fn start(
        &self,
        who: &Principal,
        request_id: &str,
        position: Option<DiscussionPosition>,
    ) -> Result<StoredDiscussion, ForgeError> {
        let proof = self.comment_proof(who).await?;
        self.review
            .start_discussion(
                &request(request_id),
                &proof,
                ready(self.repo),
                "synthetic comment",
                position.as_ref(),
            )
            .await
    }

    async fn resolve_proof(
        &self,
        who: &Principal,
        id: DiscussionId,
    ) -> Result<Authorized<DiscussionCtx>, ForgeError> {
        let mr = self
            .review
            .merge_request(self.project, self.mr.iid)
            .await
            .expect("load")
            .expect("merge request");
        let d = self
            .review
            .discussion(mr.id, id)
            .await
            .expect("load")
            .expect("discussion");
        prove(
            who,
            Action::DiscussionResolve,
            DiscussionCtx::new(
                loaded(&self.t, who, self.project).await,
                forge_review::loaded_merge_request(&mr),
                LoadedDiscussion {
                    id: d.id,
                    started_by: d.started_by,
                },
            ),
        )
    }

    async fn resolve(
        &self,
        who: &Principal,
        id: DiscussionId,
        state: bool,
    ) -> Result<ResolveOutcome, ForgeError> {
        let proof = self.resolve_proof(who, id).await?;
        self.review
            .resolve(&request("req-resolve"), &proof, state)
            .await
            .map(|(outcome, _)| outcome)
    }
}

fn at(version: i64, path: &str, side: DiscussionSide, line: i64) -> Option<DiscussionPosition> {
    Some(DiscussionPosition {
        version,
        path: path.to_owned(),
        side,
        line,
    })
}

fn token(user: UserId, kind: AccountKind) -> Principal {
    Principal::authenticated(user, false, true, Credential::Token(vec![Scope::Api]), kind)
}

fn reason(err: &ForgeError) -> String {
    err.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

/// FG-REVIEW-0004's test: a position outside the version's diff — a line
/// in no hunk, a path the diff does not list, a version the merge request
/// does not have — is refused and nothing is written; a changed line and
/// its context are accepted on their side.
#[tokio::test]
async fn an_inline_discussion_outside_the_diff_is_refused() {
    let w = world().await;
    let alice = session(w.alice);
    let new = DiscussionSide::New;
    // The hunk of v1 is lines 7..=13 on both sides (line 10 rewritten).
    let ok = w
        .start(&alice, "req-ok", at(1, "lib.rs", new, 10))
        .await
        .expect("the changed line");
    assert_eq!(ok.position, at(1, "lib.rs", new, 10));
    w.start(&alice, "req-ctx", at(1, "lib.rs", DiscussionSide::Old, 7))
        .await
        .expect("a context line on the old side");
    for (request_id, position, why) in [
        ("req-line", at(1, "lib.rs", new, 1), "line_out_of_range"),
        (
            "req-line-far",
            at(1, "lib.rs", new, 500),
            "line_out_of_range",
        ),
        ("req-path", at(1, "README.md", new, 1), "path_not_in_diff"),
        ("req-version", at(9, "lib.rs", new, 10), "version_unknown"),
    ] {
        let err = w.start(&alice, request_id, position).await.expect_err(why);
        assert_eq!(err.code(), ErrorCode::DiscussionPositionInvalid, "{why}");
        assert_eq!(reason(&err), why);
        let n = count(
            &w.t,
            &format!("SELECT count(*) AS n FROM forge.events WHERE request_id = '{request_id}'"),
        )
        .await;
        assert_eq!(n, 0, "{why}: nothing written");
    }
    // A bad shape is the request's error, before the diff is read.
    let err = w
        .start(&alice, "req-shape", at(1, "lib.rs", new, 0))
        .await
        .expect_err("line 0");
    assert_eq!(err.code(), ErrorCode::RequestInvalid);
    w.t.drop_now().await;
}

/// A new version that rewrites the commented line marks the thread
/// outdated (computed when read); threads on untouched lines, general
/// threads and threads on the current version are not.
#[tokio::test]
async fn a_discussion_is_outdated_after_a_version_changes_its_line() {
    let w = world().await;
    let alice = session(w.alice);
    let new = DiscussionSide::New;
    let on_ten = w
        .start(&alice, "req-10", at(1, "lib.rs", new, 10))
        .await
        .expect("10");
    let on_eight = w
        .start(&alice, "req-8", at(1, "lib.rs", new, 8))
        .await
        .expect("8");
    let general = w.start(&alice, "req-g", None).await.expect("general");
    let outdated = |list: &forge_review::DiscussionList, id: DiscussionId| {
        list.discussions
            .iter()
            .find(|v| v.discussion.id == id)
            .map(|v| v.outdated)
            .expect("listed")
    };
    let mr = w
        .review
        .merge_request(w.project, w.mr.iid)
        .await
        .expect("mr")
        .expect("mr");
    let before = w
        .review
        .discussions(&mr, ready(w.repo), "user:1")
        .await
        .expect("list");
    assert!(!outdated(&before, on_ten.id));
    // v2 rewrites line 10 again; line 8 stays.
    let v2 = commit(&w.path, &[(10, "ten v2")], Some(&w.v1));
    plumb(&w.path, &["update-ref", "refs/heads/feature", &v2], None);
    w.version(2, &v2).await;
    let on_v2 = w
        .start(&alice, "req-v2", at(2, "lib.rs", new, 10))
        .await
        .expect("v2");
    let mr = w
        .review
        .merge_request(w.project, w.mr.iid)
        .await
        .expect("mr")
        .expect("mr");
    assert_eq!(mr.head_sha.as_deref(), Some(v2.as_str()));
    let after = w
        .review
        .discussions(&mr, ready(w.repo), "user:1")
        .await
        .expect("list");
    assert!(outdated(&after, on_ten.id), "line 10 was rewritten");
    assert!(!outdated(&after, on_eight.id), "line 8 is untouched");
    assert!(!outdated(&after, general.id), "a general thread");
    assert!(!outdated(&after, on_v2.id), "on the current version");
    w.t.drop_now().await;
}

/// ADR-0024 §12 through the commands: an agent's token and a service
/// account comment but never resolve; the merge request's author never
/// resolves another's thread; the starter resolves their own; a Developer
/// who is not the author resolves another's. Each change emits one event.
#[tokio::test]
async fn who_may_comment_and_who_may_resolve() {
    let w = world().await;
    let rita = session(w.rita);
    let thread = w
        .start(&rita, "req-rita", None)
        .await
        .expect("a Reporter comments");
    let agent = token(w.bob, AccountKind::Human).as_agent(true);
    w.start(&agent, "req-agent", None)
        .await
        .expect("an agent comments");
    let svc = token(w.svc, AccountKind::ServiceAccount);
    w.start(&svc, "req-svc", None)
        .await
        .expect("a service account comments");
    for (who, why) in [(&agent, "agent_token"), (&svc, "service_account")] {
        let err = w.resolve(who, thread.id, true).await.expect_err(why);
        assert_eq!(
            (err.code(), reason(&err).as_str()),
            (ErrorCode::Forbidden, why)
        );
    }
    let author = session(w.alice);
    let err = w
        .resolve(&author, thread.id, true)
        .await
        .expect_err("author");
    assert_eq!(reason(&err), "author_foreign_discussion");
    assert_eq!(
        w.resolve(&rita, thread.id, true)
            .await
            .expect("the starter"),
        ResolveOutcome::Changed
    );
    assert_eq!(
        w.resolve(&session(w.bob), thread.id, false)
            .await
            .expect("a Developer, not the author"),
        ResolveOutcome::Changed
    );
    let n = count(
        &w.t,
        "SELECT count(*) AS n FROM forge.events WHERE event_type = 'merge_request.commented' \
         AND request_id = 'req-resolve'",
    )
    .await;
    assert_eq!(n, 2, "one event per resolve / unresolve");
    w.t.drop_now().await;
}

/// The second decision binds the write: a Reporter's proof taken before
/// the removal of the membership is refused inside the transaction.
#[tokio::test]
async fn a_comment_after_losing_the_role_is_refused_under_the_lock() {
    let w = world().await;
    let rita = session(w.rita);
    let proof = w.comment_proof(&rita).await.expect("proof while Reporter");
    // Demoted to Guest behind the proof's back (an audited fixture write;
    // the member commands belong to forge-hosting).
    w.t.commit_audited(
        ("project", w.project.get()),
        &[format!(
            "UPDATE forge.project_members SET role = 'guest' \
             WHERE project_id = {} AND user_id = {}",
            w.project.get(),
            w.rita.get()
        )],
    )
    .await;
    let err = w
        .review
        .start_discussion(&request("req-late"), &proof, ready(w.repo), "late", None)
        .await
        .expect_err("refused");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    w.t.drop_now().await;
}
