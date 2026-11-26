#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly
//! Plan 16 A5 (ADR-0032 Amendment A5) against a migrated database and a
//! real bare repository: a tag of the parts repository is read with git at
//! the commit it names, checked (default branch, a succeeded pipeline),
//! signed with the server key and frozen; moving the tag afterwards
//! changes nothing; promotion and revocation are audited and a revoked
//! version is final. Synthetic users, keys and parts only.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use forge_auth::{prove, AccountKind, Action, AgentPartCtx, Authorized, Credential, Principal};
use forge_core::config::AgentConfig;
use forge_core::{RequestId, UserId, Visibility};
use forge_db::change::{Actor, ChangeContext};
use forge_db::groups::{self, NewGroup};
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::test_support::{assert_reason, FreshDb};
use forge_db::users::{self, NewUser};
use forge_errors::ErrorCode;
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_marketplace::{Marketplace, MarketplaceRequest};

/// A synthetic Ed25519 seed (never a real key).
const SEED: &str = "0707070707070707070707070707070707070707070707070707070707070707";
const SLUG: &str = "guard-rule";

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

fn rid(s: &str) -> RequestId {
    RequestId::parse(s).expect("request id")
}

fn system(s: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, rid(s))
}

fn request(s: &str) -> MarketplaceRequest {
    MarketplaceRequest {
        request_id: rid(s),
        credential_id: None,
    }
}

fn session(user: UserId) -> Principal {
    Principal::authenticated(user, false, true, Credential::Session, AccountKind::Human)
}

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    _keys: tempfile::TempDir,
    browser: Browser,
    market: Marketplace,
    alice: UserId,
    bob: UserId,
    carol: UserId,
    parts: CreatedProject,
    path: PathBuf,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &system("req-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let bob = users::create(
        &t.db,
        &system("req-bob"),
        &NewUser::human("bob", "bob", "bob@example.test"),
    )
    .await
    .expect("bob");
    let carol = users::create(
        &t.db,
        &system("req-carol"),
        &NewUser::human("carol", "carol", "carol@example.test"),
    )
    .await
    .expect("carol");
    let group = groups::create(
        &t.db,
        &system("req-group"),
        &NewGroup {
            segment: "agents".to_owned(),
            parent: None,
            display_name: "agents".to_owned(),
            description: String::new(),
            owner: alice.user_id,
            visibility: Visibility::Internal,
        },
    )
    .await
    .expect("group");
    groups::add_member(
        &t.db,
        &system("req-carol-member"),
        group.group_id,
        carol.user_id,
        forge_auth::Role::Maintainer,
    )
    .await
    .expect("carol a Maintainer");
    let parts = projects::create(
        &t.db,
        &system("req-parts"),
        &NewProject::new("parts", group.namespace_id, "parts", alice.user_id),
    )
    .await
    .expect("project");
    let _ready =
        forge_db::repositories::mark_ready(&t.db, &system("req-ready"), parts.repository_id)
            .await
            .expect("ready");
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let path = storage
        .init_bare(&git, parts.repository_id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    let browser = Browser::new(git, storage, BrowseLimits::default());
    // The key file (kid 1) and its public key registered by the operator.
    let keys = tempfile::tempdir().expect("keys");
    let key_file = keys.path().join("agent-signing-keys");
    std::fs::write(&key_file, format!("1 {SEED}\n")).expect("key file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600)).expect("mode");
    }
    let seed: [u8; 32] = [7; 32];
    let public_key = forge_agent::public_key_from_seed(&seed).expect("public key");
    t.exec_superuser(&format!(
        "SELECT forge.agent_signing_key_manage('register', '1', '{public_key}', NULL, 'req-key')"
    ))
    .await;
    let config = AgentConfig {
        signing_keys_file: Some(key_file),
        parts_repository: "agents/parts".to_owned(),
        mcp_allowed_hosts: Vec::new(),
    };
    let market = Marketplace::new(t.db.clone(), browser.clone(), config);
    World {
        t,
        _home: home,
        _root: root,
        _keys: keys,
        browser,
        market,
        alice: alice.user_id,
        bob: bob.user_id,
        carol: carol.user_id,
        parts,
        path,
    }
}

impl World {
    fn tree(&self, entries: &[(&str, &str, &str)]) -> String {
        let listing: String = entries
            .iter()
            .map(|(mode, kind_oid, name)| {
                let (kind, oid) = kind_oid.split_once(' ').expect("kind oid");
                format!("{mode} {kind} {oid}\t{name}\n")
            })
            .collect();
        plumb(&self.path, &["mktree"], Some(listing.as_bytes()))
    }

    fn blob(&self, content: &str) -> String {
        plumb(
            &self.path,
            &["hash-object", "-w", "--stdin"],
            Some(content.as_bytes()),
        )
    }

    /// A commit whose `guard-rule/` holds a rule part of `version` with
    /// `body` as its rule file.
    fn part_commit(&self, version: &str, body: &str, parent: Option<&str>) -> String {
        self.part_commit_with(version, body, "", parent)
    }

    /// As [`Self::part_commit`], declaring `capabilities` (TOML list items).
    fn part_commit_with(
        &self,
        version: &str,
        body: &str,
        capabilities: &str,
        parent: Option<&str>,
    ) -> String {
        let manifest = self.blob(&format!(
            "slug = \"{SLUG}\"
kind = \"rule\"
version = \"{version}\"
capabilities = [{capabilities}]
files = [\".claude/rules/guard.md\"]
"
        ));
        let rule = self.blob(body);
        let rules = self.tree(&[("100644", &format!("blob {rule}"), "guard.md")]);
        let claude = self.tree(&[("040000", &format!("tree {rules}"), "rules")]);
        let part = self.tree(&[
            ("040000", &format!("tree {claude}"), ".claude"),
            ("100644", &format!("blob {manifest}"), "part.toml"),
        ]);
        let readme = self.blob("parts\n");
        let root = self.tree(&[
            ("100644", &format!("blob {readme}"), "README.md"),
            ("040000", &format!("tree {part}"), SLUG),
        ]);
        let message = format!("synthetic {version}");
        let mut args = vec!["commit-tree", root.as_str(), "-m", message.as_str()];
        if let Some(p) = parent {
            args.extend(["-p", p]);
        }
        plumb(&self.path, &args, None)
    }

    fn set_ref(&self, name: &str, sha: &str) {
        plumb(&self.path, &["update-ref", name, sha], None);
        self.browser.invalidate(self.parts.repository_id);
    }

    /// `pusher` pushed `tag` at `sha` (the push audit the loader reads).
    async fn pushed_tag(&self, pusher: UserId, tag: &str, sha: &str, n: usize) {
        let project = self.parts.project_id.get();
        self.t
            .exec_superuser(&format!(
                "INSERT INTO forge.audit_log (actor_kind, actor_user_id, action, outcome, \
                 resource_type, resource_id, project_id, request_id, details) VALUES ('user', {}, \
                 'repository.pushed', 'succeeded', 'project', {project}, {project}, 'req-push-{n}', \
                 '{{\"refs\": [{{\"ref\": \"refs/tags/{tag}\", \"new\": \"{sha}\", \"kind\": \"create\"}}]}}')",
                pusher.get()
            ))
            .await;
    }

    async fn pipeline(&self, sha: &str, status: &str) {
        self.t
            .exec_superuser(&format!(
                "INSERT INTO forge.ci_pipelines (project_id, ref, sha, source, status, request_id, \
                 protected, copy_mark) VALUES ({}, 'refs/heads/main', '{sha}', 'manual', \
                 '{status}', 'req-pipe', false, 'open')",
                self.parts.project_id.get(),
            ))
            .await;
    }

    /// A commit on main, tagged `<slug>/v<version>` and pushed by bob.
    async fn tagged(&self, version: &str, body: &str, n: usize) -> String {
        let sha = self.part_commit(version, body, None);
        self.set_ref("refs/heads/main", &sha);
        let tag = format!("{SLUG}/v{version}");
        self.set_ref(&format!("refs/tags/{tag}"), &sha);
        self.pushed_tag(self.bob, &tag, &sha, n).await;
        sha
    }

    async fn publish_proof(&self, version: &str) -> Authorized<AgentPartCtx> {
        let ctx = self
            .market
            .load_for_publish(SLUG, &format!("{SLUG}/v{version}"), Some(self.alice))
            .await
            .expect("load")
            .expect("tag found");
        prove(&session(self.alice), Action::AgentPartPublish, ctx).expect("alice may publish")
    }

    /// `who`'s proof of `action` on a stored version.
    async fn version_proof(
        &self,
        who: UserId,
        action: Action,
        version: &str,
    ) -> Authorized<AgentPartCtx> {
        let ctx = self
            .market
            .load_version(SLUG, version, Some(who))
            .await
            .expect("load")
            .expect("version found");
        prove(&session(who), action, ctx).expect("allowed")
    }
}

/// Plan 16 A5: nothing is signed or written until the newest pipeline on
/// the tagged commit succeeded (`FG-AGENT-0004`); then the version is
/// frozen as beta, signed by kid 1, and the signature verifies.
#[tokio::test]
async fn publish_refused_when_pipeline_not_green() {
    let w = world().await;
    let sha = w.tagged("1.0.0", "# Guard\n", 1).await;
    let proof = w.publish_proof("1.0.0").await;
    assert_eq!(proof.ctx().view().tag_pusher, Some(w.bob));

    let err = w
        .market
        .publish(&request("req-pub-1"), &proof)
        .await
        .expect_err("no pipeline");
    assert_reason(&err, ErrorCode::PartChecksNotGreen, "pipeline_missing");

    w.pipeline(&sha, "failed").await;
    let err = w
        .market
        .publish(&request("req-pub-2"), &proof)
        .await
        .expect_err("failed pipeline");
    assert_reason(&err, ErrorCode::PartChecksNotGreen, "pipeline_not_green");
    assert!(w.market.part(SLUG).await.expect("read").is_none());

    w.pipeline(&sha, "succeeded").await;
    let published = w
        .market
        .publish(&request("req-pub-3"), &proof)
        .await
        .expect("published");
    let v = &published.version;
    assert_eq!(
        (v.channel.as_str(), v.state.as_str(), v.key_id.as_str()),
        ("beta", "published", "1")
    );
    assert_eq!(v.source_sha, sha);
    assert_eq!(published.files.len(), 1);
    let ring = forge_agent::KeyRing::parse(
        format!(
            "{{\"keys\":[{{\"kid\":\"1\",\"public_key\":\"{}\"}}]}}",
            forge_agent::public_key_from_seed(&[7; 32]).expect("pk")
        )
        .as_bytes(),
    )
    .expect("ring");
    let digest = forge_agent::Digest::parse(&v.digest).expect("digest");
    forge_agent::verify_signature(&ring, "1", &digest, &v.signature).expect("signature verifies");
}

/// Plan 16 A5: a published version is frozen — the tag moved to another
/// commit changes nothing that was published, and publishing the version
/// again is `FG-AGENT-0012`.
#[tokio::test]
async fn moved_tag_after_publish_has_no_effect() {
    let w = world().await;
    let first = w.tagged("1.0.0", "# Guard\n", 1).await;
    w.pipeline(&first, "succeeded").await;
    let proof = w.publish_proof("1.0.0").await;
    let before = w
        .market
        .publish(&request("req-pub-1"), &proof)
        .await
        .expect("published");

    let second = w.part_commit("1.0.0", "# Guard, rewritten\n", Some(&first));
    w.set_ref("refs/heads/main", &second);
    w.set_ref(&format!("refs/tags/{SLUG}/v1.0.0"), &second);
    w.pushed_tag(w.bob, &format!("{SLUG}/v1.0.0"), &second, 2)
        .await;
    w.pipeline(&second, "succeeded").await;

    let after = w
        .market
        .version(SLUG, "1.0.0")
        .await
        .expect("read")
        .expect("version");
    assert_eq!(after, before);
    assert_eq!(after.version.source_sha, first);

    let again = w.publish_proof("1.0.0").await;
    assert_eq!(again.ctx().target().source_sha, second);
    let err = w
        .market
        .publish(&request("req-pub-2"), &again)
        .await
        .expect_err("already published");
    assert_reason(&err, ErrorCode::PartVersionExists, "version_exists");
}

/// Plan 16 A5: a tag of a commit that never reached the default branch is
/// refused before anything is signed.
#[tokio::test]
async fn publish_refused_for_a_commit_off_the_default_branch() {
    let w = world().await;
    let main = w.part_commit("0.9.0", "# Old\n", None);
    w.set_ref("refs/heads/main", &main);
    let side = w.part_commit("1.0.0", "# Side\n", None);
    w.set_ref(&format!("refs/tags/{SLUG}/v1.0.0"), &side);
    w.pushed_tag(w.bob, &format!("{SLUG}/v1.0.0"), &side, 1)
        .await;
    w.pipeline(&side, "succeeded").await;
    let proof = w.publish_proof("1.0.0").await;
    let err = w
        .market
        .publish(&request("req-pub-1"), &proof)
        .await
        .expect_err("off main");
    assert_reason(&err, ErrorCode::PartChecksNotGreen, "not_on_default_branch");
}

/// Plan 16 A5: beta -> stable once; a revoked version is final
/// (`FG-AGENT-0006`) for promotion and for a second revocation; the
/// facts a promotion decides on are the publication's.
#[tokio::test]
async fn promote_of_a_revoked_version_is_refused() {
    let w = world().await;
    for (n, version) in ["1.0.0", "1.1.0"].iter().enumerate() {
        let sha = w.tagged(version, &format!("# Guard {version}\n"), n).await;
        w.pipeline(&sha, "succeeded").await;
        let proof = w.publish_proof(version).await;
        w.market
            .publish(&request(&format!("req-pub-{n}")), &proof)
            .await
            .expect("published");
    }

    // The publisher does not promote alone (review A5 LOW): alice is
    // refused, carol (another Maintainer) promotes.
    let ctx = w
        .market
        .load_version(SLUG, "1.1.0", Some(w.alice))
        .await
        .expect("load")
        .expect("found");
    let err = prove(&session(w.alice), Action::AgentPartPromote, ctx).expect_err("publisher");
    assert_eq!(err.code(), ErrorCode::PublishSelfApproval);
    let promote = w
        .version_proof(w.carol, Action::AgentPartPromote, "1.1.0")
        .await;
    assert_eq!(promote.ctx().view().tag_pusher, Some(w.bob));
    let (changed, detail) = w
        .market
        .promote(&request("req-promote-1"), &promote)
        .await
        .expect("promoted");
    assert!(changed);
    assert_eq!(detail.version.channel, "stable");
    let (changed, _) = w
        .market
        .promote(&request("req-promote-2"), &promote)
        .await
        .expect("stable already");
    assert!(!changed);

    let revoke = w
        .version_proof(w.alice, Action::AgentPartRevoke, "1.0.0")
        .await;
    let revoked = w
        .market
        .revoke(&request("req-revoke-1"), &revoke, "synthetic: superseded")
        .await
        .expect("revoked");
    assert_eq!(revoked.version.state, "revoked");
    assert_eq!(revoked.version.revoked_by, Some(w.alice.get()));
    let err = w
        .market
        .revoke(&request("req-revoke-2"), &revoke, "again")
        .await
        .expect_err("final");
    assert_reason(&err, ErrorCode::PartVersionRevoked, "revoked");
    let promote = w
        .version_proof(w.carol, Action::AgentPartPromote, "1.0.0")
        .await;
    let err = w
        .market
        .promote(&request("req-promote-3"), &promote)
        .await
        .expect_err("revoked");
    assert_reason(&err, ErrorCode::PartVersionRevoked, "revoked");

    let catalog = w.market.list(None, None, None).await.expect("list");
    assert_eq!(catalog.len(), 1);
    let latest = catalog[0].latest.as_ref().expect("live version");
    assert_eq!(latest.version, "1.1.0");
}

/// Plan 16 §4 (A4 review HIGH, A5 loader): the pusher comes from the push
/// audit only; a tag nobody is recorded to have pushed leaves it unknown
/// and authorize() refuses (fails closed).
#[tokio::test]
async fn a_tag_without_a_recorded_push_cannot_be_published() {
    let w = world().await;
    let sha = w.part_commit("1.0.0", "# Guard\n", None);
    w.set_ref("refs/heads/main", &sha);
    w.set_ref(&format!("refs/tags/{SLUG}/v1.0.0"), &sha);
    let ctx = w
        .market
        .load_for_publish(SLUG, &format!("{SLUG}/v1.0.0"), Some(w.alice))
        .await
        .expect("load")
        .expect("tag found");
    assert_eq!(ctx.view().tag_pusher, None);
    let err = prove(&session(w.alice), Action::AgentPartPublish, ctx).expect_err("refused");
    assert_reason(&err, ErrorCode::Forbidden, "tag_pusher_unknown");
    // Whoever else pushed the same tag later is one of the people behind
    // it too (review A5 MEDIUM): bob first, then alice -> alice refused.
    w.pushed_tag(w.bob, &format!("{SLUG}/v1.0.0"), &sha, 1)
        .await;
    w.pushed_tag(w.alice, &format!("{SLUG}/v1.0.0"), &sha, 2)
        .await;
    let ctx = w
        .market
        .load_for_publish(SLUG, &format!("{SLUG}/v1.0.0"), Some(w.alice))
        .await
        .expect("load")
        .expect("tag found");
    let err = prove(&session(w.alice), Action::AgentPartPublish, ctx).expect_err("own tag");
    assert_eq!(err.code(), ErrorCode::PublishSelfApproval);
}

/// Review A5 HIGH: the loader does not read the part; `publish` decides
/// again on the part's own `network` declaration (an administrator's)
/// before anything is checked or signed.
#[tokio::test]
async fn a_network_part_is_decided_again_before_signing() {
    let w = world().await;
    let sha = w.part_commit_with(
        "1.0.0",
        "# Guard
",
        "\"network\"",
        None,
    );
    w.set_ref("refs/heads/main", &sha);
    w.set_ref(&format!("refs/tags/{SLUG}/v1.0.0"), &sha);
    w.pushed_tag(w.bob, &format!("{SLUG}/v1.0.0"), &sha, 1)
        .await;
    w.pipeline(&sha, "succeeded").await;
    let proof = w.publish_proof("1.0.0").await;
    let err = w
        .market
        .publish(&request("req-pub-1"), &proof)
        .await
        .expect_err("not an administrator");
    assert_reason(&err, ErrorCode::Forbidden, "network_capability");
    assert!(w.market.part(SLUG).await.expect("read").is_none());
}
