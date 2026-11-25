#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The linked wiki the document lint checks `[[wiki:…]]` against (plan 20
//! W3, ADR-0040 Amendment W3) on the development database and real bare
//! repositories: linked, not linked, more private (withheld), not ready,
//! bounded, and a wiki whose storage cannot be read (`read_failed`, never
//! a job error). Synthetic data only.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use forge_checks::doc_lint::{DocLimits, DocTree};
use forge_checks::doc_wiki::{read_wiki, WikiRead, WikiReadFor};
use forge_core::{ProjectId, RepoId, RequestId, Visibility};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_doc::lint::WikiRefs;
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};

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

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

const PAGE: &[u8] =
    b"---\ndoctype: wiki.page\nid: WIKI-0001\ntitle: \"t\"\nowner: \"o\"\n---\n\nx\n";

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    git: Git,
    storage: StorageRoot,
    browser: Browser,
    owner: ProjectId,
    wiki: ProjectId,
    wiki_repo: RepoId,
    request: RequestId,
}

/// The owner (`owner_visibility`) and its wiki (`wiki_visibility`), the
/// wiki's repository NOT ready and the two NOT linked yet.
async fn world(owner_visibility: Visibility, wiki_visibility: Visibility) -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &system("req-user-alice"),
        &NewUser::human("alice", "Alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let mut new = NewProject::new("web", alice.namespace_id, "web", alice.user_id);
    new.visibility = owner_visibility;
    let web = projects::create(&t.db, &system("req-web"), &new)
        .await
        .expect("web");
    let mut new = NewProject::new("web-wiki", alice.namespace_id, "Web Wiki", alice.user_id);
    new.visibility = wiki_visibility;
    let wiki = projects::create(&t.db, &system("req-wiki"), &new)
        .await
        .expect("wiki");
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    World {
        browser: Browser::new(git.clone(), storage.clone(), BrowseLimits::default()),
        git,
        storage,
        _home: home,
        _root: root,
        owner: web.project_id,
        wiki: wiki.project_id,
        wiki_repo: wiki.repository_id,
        request: RequestId::parse("req-doc-lint-wiki").expect("id"),
        t,
    }
}

impl World {
    async fn link(&self) {
        self.t
            .commit_audited(
                ("project", self.owner.get()),
                &[format!(
                    "INSERT INTO forge.project_wikis (project_id, wiki_project_id, linked_by) \
                     SELECT {}, {}, id FROM forge.users LIMIT 1",
                    self.owner.get(),
                    self.wiki.get()
                )],
            )
            .await;
    }

    /// The wiki's repository ready, with one page on `main`.
    async fn seed(&self) -> std::path::PathBuf {
        let _ready =
            forge_db::repositories::mark_ready(&self.t.db, &system("req-ready"), self.wiki_repo)
                .await
                .expect("ready");
        let path = self
            .storage
            .init_bare(&self.git, self.wiki_repo, uuid::Uuid::new_v4(), None)
            .await
            .expect("init");
        let blob = plumb(&path, &["hash-object", "-w", "--stdin"], Some(PAGE));
        let tree = plumb(
            &path,
            &["mktree"],
            Some(format!("100644 blob {blob}\tWIKI-0001.md\n").as_bytes()),
        );
        let commit = plumb(&path, &["commit-tree", &tree, "-m", "synthetic"], None);
        plumb(&path, &["update-ref", "refs/heads/main", &commit], None);
        plumb(&path, &["symbolic-ref", "HEAD", "refs/heads/main"], None);
        path
    }

    async fn read(&self, visibility: Visibility, limits: DocLimits) -> WikiRead {
        let mut docs = DocTree::default();
        docs.add("N-1.md", b"see [[wiki:WIKI-0001]]".to_vec());
        read_wiki(
            (&self.t.db, &self.browser, &self.t.db),
            WikiReadFor {
                project: self.owner,
                visibility,
                request_id: &self.request,
            },
            &docs,
            (&limits, Duration::from_secs(5)),
        )
        .await
    }
}

#[tokio::test]
async fn read_wiki_answers_each_state_and_never_fails() {
    let w = world(Visibility::Private, Visibility::Private).await;
    let d = DocLimits::default();
    assert_eq!(w.read(Visibility::Private, d).await.state, "not_linked");
    w.link().await;
    let not_ready = w.read(Visibility::Private, d).await;
    assert_eq!(not_ready.state, "not_ready");
    assert_eq!(not_ready.refs, WikiRefs::Unchecked);
    let note = not_ready.note.expect("note");
    assert!(!note.contains(&w.wiki.get().to_string()), "{note}");
    assert!(!note.contains("provisioning"), "{note}");
    let path = w.seed().await;
    let linked = w.read(Visibility::Private, d).await;
    assert_eq!(linked.state, "linked");
    let WikiRefs::Linked(index) = &linked.refs else {
        panic!("{linked:?}");
    };
    assert!(index.resolves("WIKI-0001", None));
    let tiny = DocLimits {
        max_files: 0,
        ..DocLimits::default()
    };
    assert_eq!(w.read(Visibility::Private, tiny).await.state, "bounded");
    // The storage gone: the read fails, the job does not.
    std::fs::remove_dir_all(&path).expect("remove");
    let failed = w.read(Visibility::Private, d).await;
    assert_eq!(failed.state, "read_failed", "{failed:?}");
    assert_eq!(failed.refs, WikiRefs::Unchecked);
}

#[tokio::test]
async fn a_wiki_more_private_than_the_project_is_withheld() {
    let w = world(Visibility::Internal, Visibility::Private).await;
    w.link().await;
    let _path = w.seed().await;
    let r = w.read(Visibility::Internal, DocLimits::default()).await;
    assert_eq!(r.state, "withheld");
    assert_eq!(r.refs, WikiRefs::Unchecked);
    assert_eq!(
        r.note.as_deref(),
        Some(forge_checks::doc_wiki::WITHHELD_NOTE)
    );
}
