//! Plan 16 A9 (ADR-0032 Amendment A9) against a migrated database and a
//! real bare repository, with forge-review's `Review::open` as the opener
//! (as `forge serve` wires it): revoking a part version queues one update
//! job per project whose lock uses it, and the job opens ONE update merge
//! request (run twice: the same one) for the project's builder. Synthetic
//! users, keys and parts only.
// Plan 16 A3 review: tests sign with a synthetic seed (clippy.toml).
#![allow(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use forge_core::config::AgentConfig;
use forge_core::{ProjectId, RepoId, RequestId, UserId, Visibility};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::ErrorCode;
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_marketplace::{
    agent_update_payload, AgentUpdater, BuildOutcome, Marketplace, Selected, UpdateReport,
    AGENT_UPDATE_JOB,
};

use super::ReviewOpener;

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
    git: Git,
    storage: StorageRoot,
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
    let browser = Browser::new(git.clone(), storage.clone(), BrowseLimits::default());
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
        git,
        storage,
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

impl World {
    /// Build `selection` onto `main` (the configuration merged) and record
    /// the finished build for `by`.
    async fn built(&self, selection: &[Selected], by: UserId) {
        let plan = self
            .market
            .plan_build(self.project, self.repo_id, selection)
            .await
            .expect("plan");
        // The whole configuration (`main_with` writes a tree of exactly
        // these files).
        let files: Vec<(String, String)> = plan
            .compiled
            .written()
            .into_iter()
            .map(|(p, b)| (p, String::from_utf8(b).expect("text")))
            .collect();
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();
        self.main_with(&refs);
        self.market
            .record_build(
                self.project,
                by,
                Some(&plan),
                &plan.selection,
                false,
                &BuildOutcome::Succeeded {
                    lock_digest: &plan.compiled.lock_digest,
                    merge_request: None,
                },
                "req-built",
                "build",
            )
            .await
            .expect("record");
    }

    fn updater(&self) -> AgentUpdater {
        AgentUpdater::new(
            self.market.clone(),
            self.git.clone(),
            self.storage.clone(),
            Arc::new(ReviewOpener(forge_review::Review::new(
                self.t.db.clone(),
                Arc::new(self.browser.clone()),
            ))),
        )
    }

    async fn update_jobs(&self) -> Vec<serde_json::Value> {
        self.t
            .json_rows(&format!(
                "SELECT payload, dedup_key FROM forge.jobs WHERE kind = '{AGENT_UPDATE_JOB}' ORDER BY id"
            ))
            .await
    }

    async fn revoke(&self, version: &str) -> Result<usize, forge_errors::ForgeError> {
        forge_db::agent_marketplace::revoke_version(
            &self.t.db,
            &ctx(
                Some(self.alice),
                &format!("req-revoke-{}", version.replace('.', "-")),
            ),
            "format-on-save",
            version,
            self.alice,
            "synthetic",
        )
        .await
    }
}

fn rid(s: &str) -> RequestId {
    RequestId::parse(s).expect("request id")
}

#[tokio::test]
async fn revoke_opens_update_mr_once() {
    let w = world().await;
    w.base_parts().await;
    w.main_with(&[("README.md", "synthetic\n")]);
    w.built(
        &[
            pick("claude-code-v2", "1.0.0"),
            pick("format-on-save", "1.0.0"),
        ],
        w.alice,
    )
    .await;

    // One job per project whose lock uses it, in the revocation's own
    // transaction.
    assert_eq!(w.revoke("1.0.0").await.expect("revoke"), 1);
    let jobs = w.update_jobs().await;
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    let asked = agent_update_payload(&jobs[0]["payload"]).expect("payload");
    assert_eq!(asked.project, w.project);
    assert_eq!(
        asked.revoked,
        Some(("format-on-save".to_owned(), "1.0.0".to_owned()))
    );
    // A revocation is final: no second fan-out.
    let again = w.revoke("1.0.0").await.expect_err("final");
    assert_eq!(again.code(), ErrorCode::PartVersionRevoked);
    assert_eq!(w.update_jobs().await.len(), 1);

    // The job, run twice (a retry, a second worker): ONE merge request.
    let updater = w.updater();
    let first = updater.run(&asked, &rid("req-job-1")).await.expect("run");
    let UpdateReport::Opened { iid } = first else {
        panic!("not opened: {first:?}");
    };
    let second = updater
        .run(&asked, &rid("req-job-2"))
        .await
        .expect("run again");
    assert_eq!(second, UpdateReport::AlreadyOpen { iid });
    let mrs =
        w.t.json_rows(&format!(
            "SELECT iid, author_id, source_branch, target_branch, state \
             FROM forge.merge_requests WHERE project_id = {}",
            w.project.get()
        ))
        .await;
    assert_eq!(mrs.len(), 1, "{mrs:?}");
    assert_eq!(mrs[0]["author_id"], w.alice.get(), "opened for the builder");
    assert_eq!(mrs[0]["target_branch"], "refs/heads/main");
    let branch = mrs[0]["source_branch"].as_str().expect("branch").to_owned();
    assert!(branch.starts_with("refs/heads/agent-builder/"), "{branch}");
    // The new lock on the branch uses the replacement, not the revoked one.
    let tip = w
        .git
        .read_ref(
            &w.storage,
            w.repo_id,
            &forge_core::RefName::parse(&branch).expect("ref"),
            std::time::Duration::from_secs(20),
        )
        .await
        .expect("read")
        .expect("branch");
    let at_tip = w
        .browser
        .agent_config(w.repo_id, tip.as_str(), 2000, 8 * 1024 * 1024)
        .await
        .expect("config");
    let lock = forge_agent::Lock::parse(&at_tip.lock.expect("lock")).expect("lock");
    let hook = lock
        .parts
        .iter()
        .find(|p| p.slug == "format-on-save")
        .expect("hook");
    assert_eq!(hook.version, "1.1.0");
    // Told once (the event deduplicated by the merge request), recorded once.
    let events =
        w.t.json_rows(
            "SELECT payload FROM forge.events \
             WHERE event_type = 'merge_request.agent_update_opened'",
        )
        .await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["payload"]["origin"], "revocation");
    assert_eq!(events[0]["payload"]["revoked"], "format-on-save@1.0.0");
    let builds =
        w.t.json_rows("SELECT origin, state FROM forge.agent_builds WHERE origin = 'revocation'")
            .await;
    assert_eq!(builds.len(), 1, "{builds:?}");
    w.t.drop_now().await;
}
