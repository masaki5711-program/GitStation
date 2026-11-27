#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly
// Plan 16 A3 review: tests sign with a synthetic seed (clippy.toml).
#![allow(clippy::disallowed_methods)]
//! Plan 16 A7 (ADR-0032 Amendment A7) against a migrated database and a
//! real bare repository: the builder's plan reads published versions,
//! verifies and builds them, and compares the result with the default
//! branch's tip; a revoked or missing version is refused; the
//! recommendation picks the base model and what it requires. Synthetic
//! users, keys and parts only.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use forge_core::config::AgentConfig;
use forge_core::{ProjectId, RepoId, RequestId, UserId, Visibility};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::ErrorCode;
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_marketplace::{Marketplace, Selected};

const SEED: [u8; 32] = [7u8; 32];

fn plumb(repo: &Path, index: &Path, args: &[&str], input: Option<&[u8]>) -> String {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .env("GIT_INDEX_FILE", index)
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
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn ctx(user: Option<UserId>, s: &str) -> ChangeContext {
    let actor = user.map_or(Actor::System, Actor::User);
    ChangeContext::new(actor, RequestId::parse(s).expect("request id"))
}

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    scratch: tempfile::TempDir,
    browser: Browser,
    market: Marketplace,
    alice: UserId,
    project: ProjectId,
    repo_id: RepoId,
    repo: PathBuf,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &ctx(None, "req-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let mut new = NewProject::new("app", alice.namespace_id, "app", alice.user_id);
    new.visibility = Visibility::Private;
    let app = projects::create(&t.db, &ctx(None, "req-app"), &new)
        .await
        .expect("project");
    let _ready =
        forge_db::repositories::mark_ready(&t.db, &ctx(None, "req-ready"), app.repository_id)
            .await
            .expect("ready");
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let repo = storage
        .init_bare(&git, app.repository_id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    let browser = Browser::new(git, storage, BrowseLimits::default());
    let public = forge_agent::public_key_from_seed(&SEED).expect("public key");
    t.exec_superuser(&format!(
        "SELECT forge.agent_signing_key_manage('register', '1', '{public}', NULL, 'req-key')"
    ))
    .await;
    let market = Marketplace::new(
        t.db.clone(),
        browser.clone(),
        AgentConfig {
            signing_keys_file: None,
            parts_repository: "agents/parts".to_owned(),
            mcp_allowed_hosts: Vec::new(),
        },
    );
    World {
        t,
        _home: home,
        _root: root,
        scratch: tempfile::tempdir().expect("scratch"),
        browser,
        market,
        alice: alice.user_id,
        project: app.project_id,
        repo_id: app.repository_id,
        repo,
    }
}

impl World {
    /// A commit of `files` on `main`.
    fn main_with(&self, files: &[(&str, &str)]) -> String {
        let index = self
            .scratch
            .path()
            .join(format!("index-{}", uuid::Uuid::new_v4()));
        for (path, content) in files {
            let object = plumb(
                &self.repo,
                &index,
                &["hash-object", "-w", "--stdin"],
                Some(content.as_bytes()),
            );
            plumb(
                &self.repo,
                &index,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("100644,{object},{path}"),
                ],
                None,
            );
        }
        let tree = plumb(&self.repo, &index, &["write-tree"], None);
        let commit = plumb(
            &self.repo,
            &index,
            &["commit-tree", tree.as_str(), "-m", "synthetic"],
            None,
        );
        plumb(
            &self.repo,
            &index,
            &["update-ref", "refs/heads/main", &commit],
            None,
        );
        plumb(
            &self.repo,
            &index,
            &["symbolic-ref", "HEAD", "refs/heads/main"],
            None,
        );
        self.browser.invalidate(self.repo_id);
        commit
    }

    /// Publish a signed part `slug@version` (stable when `stable`).
    async fn publish(
        &self,
        slug: &str,
        kind: &str,
        version: &str,
        requires: &[&str],
        files: &[(&str, &str)],
        stable: bool,
    ) {
        let quoted = |items: &[&str]| {
            items
                .iter()
                .map(|i| format!("\"{i}\""))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let paths: Vec<&str> = files.iter().map(|(p, _)| *p).collect();
        let manifest = format!(
            "slug = \"{slug}\"\nkind = \"{kind}\"\nversion = \"{version}\"\nrequires = [{}]\nfiles = [{}]\n",
            quoted(requires),
            quoted(&paths)
        );
        let mut entries = vec![forge_agent::TreeEntry::file(
            forge_agent::MANIFEST,
            manifest,
        )];
        for (p, c) in files {
            entries.push(forge_agent::TreeEntry::file(p, *c));
        }
        let part = forge_agent::parse_part(&entries).expect("part");
        let signature = forge_agent::sign_digest(&SEED, &part.digest).expect("sign");
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        forge_db::agent_marketplace::publish_version(
            &self.t.db,
            &ctx(Some(self.alice), &format!("req-pub-{slug}-{}", version.replace('.', "-"))),
            &forge_db::agent_marketplace::NewPartVersion {
                slug: slug.to_owned(),
                kind: kind.to_owned(),
                source_project: self.project,
                version: version.to_owned(),
                source_sha: "a".repeat(40),
                tag: format!("{slug}/v{version}"),
                tag_object: "a".repeat(40),
                tag_pusher: self.alice,
                tag_authors: Vec::new(),
                manifest: serde_json::json!({"slug": slug, "kind": kind, "version": version, "requires": requires}),
                digest: part.digest.to_string(),
                signature,
                key_id: "1".to_owned(),
                capabilities: Vec::new(),
                published_by: self.alice,
                ci_pipeline_id: 1,
                files: part
                    .files
                    .iter()
                    .map(|f| forge_db::agent_marketplace::NewPartFile {
                        path: f.path.clone(),
                        mode: "100644",
                        sha256: hex(&f.sha256),
                        content: f.content.clone(),
                    })
                    .collect(),
            },
        )
        .await
        .expect("publish");
        if stable {
            forge_db::agent_marketplace::set_channel_stable(
                &self.t.db,
                &ctx(
                    Some(self.alice),
                    &format!("req-stable-{slug}-{}", version.replace('.', "-")),
                ),
                slug,
                version,
            )
            .await
            .expect("stable");
        }
    }

    async fn base_parts(&self) {
        self.publish(
            "claude-code-v2",
            "adapter",
            "1.0.0",
            &["format-on-save@^1"],
            &[
                ("CLAUDE.md", "# Synthetic\n"),
                (".claude/settings.json", r#"{"model":"opus"}"#),
            ],
            true,
        )
        .await;
        self.publish(
            "format-on-save",
            "hook",
            "1.0.0",
            &[],
            &[
                (".claude/hooks/fmt.js", "process.exit(0);\n"),
                (
                    ".claude/settings.json",
                    r#"{"hooks":{"PostToolUse":[{"hooks":[{"type":"command","command":"node .claude/hooks/fmt.js"}]}]}}"#,
                ),
            ],
            true,
        )
        .await;
        self.publish(
            "format-on-save",
            "hook",
            "1.1.0",
            &[],
            &[(".claude/hooks/fmt.js", "process.exit(0);\n")],
            true,
        )
        .await;
    }
}

fn pick(slug: &str, version: &str) -> Selected {
    Selected {
        slug: slug.to_owned(),
        version: version.to_owned(),
        selected_by: "user".to_owned(),
        reason: None,
    }
}

#[tokio::test]
async fn the_plan_builds_published_parts_against_the_tip() {
    let w = world().await;
    w.base_parts().await;
    w.main_with(&[
        ("README.md", "synthetic\n"),
        ("CLAUDE.md", "# hand written\n"),
        (".claude/commands/old.md", "# old\n"),
    ]);
    let plan = w
        .market
        .plan_build(
            w.project,
            w.repo_id,
            &[
                pick("claude-code-v2", "1.0.0"),
                pick("format-on-save", "1.0.0"),
            ],
        )
        .await
        .expect("plan");
    assert_eq!(plan.target.as_str(), "refs/heads/main");
    let changes: Vec<(&str, &str)> = plan
        .changes
        .iter()
        .map(|c| (c.path.as_str(), c.change))
        .collect();
    assert_eq!(
        changes,
        [
            (".claude/commands/old.md", "removed"),
            (".claude/hooks/fmt.js", "added"),
            (".claude/settings.json", "added"),
            (".gitstation/agent.lock.json", "added"),
            ("CLAUDE.md", "modified"),
        ]
    );
    assert_eq!(plan.removes, [".claude/commands/old.md"]);
    assert!(plan.adapter_version_id.is_some());
    // The same plan again is the same bytes (deterministic).
    let again = w
        .market
        .plan_build(
            w.project,
            w.repo_id,
            &[
                pick("format-on-save", "1.0.0"),
                pick("claude-code-v2", "1.0.0"),
            ],
        )
        .await
        .expect("plan");
    assert_eq!(again.compiled, plan.compiled);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_missing_or_revoked_version_is_refused() {
    let w = world().await;
    w.base_parts().await;
    w.main_with(&[("README.md", "synthetic\n")]);
    let err = w
        .market
        .plan_build(
            w.project,
            w.repo_id,
            &[
                pick("claude-code-v2", "1.0.0"),
                pick("format-on-save", "9.9.9"),
            ],
        )
        .await
        .expect_err("missing");
    assert_eq!(err.code(), ErrorCode::SelectionUnresolved);
    forge_db::agent_marketplace::revoke_version(
        &w.t.db,
        &ctx(Some(w.alice), "req-revoke"),
        "format-on-save",
        "1.0.0",
        w.alice,
        "synthetic",
    )
    .await
    .expect("revoke");
    let err = w
        .market
        .plan_build(
            w.project,
            w.repo_id,
            &[
                pick("claude-code-v2", "1.0.0"),
                pick("format-on-save", "1.0.0"),
            ],
        )
        .await
        .expect_err("revoked");
    assert_eq!(err.code(), ErrorCode::PartVersionRevoked);
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_recommendation_is_the_base_model_and_its_requirements() {
    let w = world().await;
    w.base_parts().await;
    w.main_with(&[("README.md", "synthetic\n")]);
    let r = w.market.recommend_for(w.repo_id).await.expect("recommend");
    let got: Vec<(&str, &str, &str)> = r
        .parts
        .iter()
        .map(|p| (p.slug.as_str(), p.version.as_str(), p.reason.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("claude-code-v2", "1.0.0", "base_model"),
            ("format-on-save", "1.1.0", "required_by=claude-code-v2"),
        ]
    );
    w.t.drop_now().await;
}
