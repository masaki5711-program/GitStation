//! forge's agent marketplace (plan 16 §2, A5; ADR-0032 Amendment A5): parts
//! are made in git and frozen into the database when a person publishes a
//! tag.
//!
//! **Publishing** (`AgentPartPublish`, a web session of a Maintainer of the
//! publishing group who is none of the people behind the tag):
//!
//! 1. [`Marketplace::load_for_publish`] (the route's loader) finds the
//!    parts project (`agent.parts_repository`, default `marketplace/parts`)
//!    and its group, the tag `<slug>/v<version>` with git, the person who
//!    pushed it (the push audit: never the self-declared tagger), the
//!    accounts behind it (tagger, author, committer by e-mail) and whether
//!    — the facts `authorize()` decides on (the part's `network`
//!    declaration is decided again by `publish` once the part is read) —
//!    and **pins the commit** the tag names now.
//! 2. [`Marketplace::publish`] reads the part at that pinned commit
//!    (`<slug>/` of the tree), lints and scans it with forge-agent
//!    (`parse_part`, `scan`: the same code as `gstn agent verify`), checks
//!    that the commit is on the default branch (it went through a merge
//!    request) and that the newest pipeline on it succeeded
//!    (`FG-AGENT-0004`), signs the digest with the server key (the file's
//!    highest kid, registered and current: `FG-AGENT-0014` otherwise),
//!    verifies the signature with the registered public key, and writes the
//!    part, the version (`beta`), its files and the audit row in one
//!    transaction. A published version never changes: moving the tag
//!    afterwards has no effect, and publishing the version again is
//!    `FG-AGENT-0012`.
//!
//! **Promoting** (beta -> stable, `AgentPartPromote`: the same people as
//! publishing, decided on the facts the publication recorded) and
//! **revoking** (`AgentPartRevoke`: a Maintainer of the group or an
//! administrator, a reason 1..=500 characters; final) are audited changes
//! of the version's row.
//!
//! The server-decided hook test job of plan 16 A5 is not built yet
//! (ADR-0032 Amendment A5: the follow-up A5b); until then the pipeline of
//! the parts repository is what must be green.

use std::time::Duration;

use forge_agent::{
    parse_part, scan, worst, Capability, EntryKind, KeyRing, Part, Problem, ScanPolicy, TreeEntry,
    MAX_PART_BYTES, MAX_PART_FILES,
};
use forge_auth::{
    AccountKind, Action, AgentPartCtx, AgentPartTarget, AgentPartView, AgentResource, Authorized,
    Principal, Resource, Role,
};
use forge_core::config::AgentConfig;
use forge_core::{
    GroupId, ObjectId, ProjectId, RefName, RepoId, RepositoryState, RequestId, UserId,
};
use forge_db::agent_marketplace::{
    self as db, NewPartFile, NewPartVersion, PartSummary, StoredFile, StoredSigningKey,
    StoredVersion,
};
use forge_db::change::{Actor, ChangeContext};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{Browser, EntryKind as GitKind, ListedBlob, RefLookup, RepoPath};

/// The stored rows the routes show (forge-web does not depend on
/// forge-db).
pub use forge_db::agent_marketplace::{
    ListedSigningKey, PartSummary as StoredPartSummary, StoredFile as StoredPartFile,
    StoredVersion as StoredPartVersion,
};

/// The most entries a part directory may list before it is refused
/// (`part.toml` and up to [`MAX_PART_FILES`] files).
const MAX_ENTRIES: usize = MAX_PART_FILES + 1;

/// `part.toml` may add this much to the files' bytes.
const MANIFEST_ALLOWANCE: u64 = 64 * 1024;

/// One bound for the whole publish step's reads and writes.
pub const PUBLISH_TIMEOUT: Duration = Duration::from_secs(60);

/// Who asks (for the change's context).
#[derive(Debug, Clone)]
pub struct MarketplaceRequest {
    pub request_id: RequestId,
    pub credential_id: Option<i64>,
}

/// The parts project, its repository and its group (the publishing group).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartsSource {
    pub project: ProjectId,
    pub repo: RepoId,
    pub group: GroupId,
}

/// A version as published (the stored row and its files).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionDetail {
    pub version: StoredVersion,
    pub files: Vec<StoredFile>,
}

/// The marketplace (forge-web's routes, forge-cli's wiring).
#[derive(Clone)]
pub struct Marketplace {
    db: Db,
    browse: Browser,
    config: AgentConfig,
}

fn manifest_invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::PartManifestInvalid).with_context("reason", reason)
}

fn checks_not_green(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::PartChecksNotGreen).with_context("reason", reason)
}

fn key_file(reason: &'static str) -> ForgeError {
    forge_core::agent_signing::key_file_invalid(reason)
}

fn internal(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", reason)
}

/// The most serious of `problems` as the error (all of them counted).
fn problems_error(problems: &[Problem]) -> ForgeError {
    let Some(first) = worst(problems) else {
        return internal("no_problem");
    };
    let mut e = ForgeError::new(first.code)
        .with_context("reason", first.reason)
        .with_context("problems", problems.len());
    if let Some(subject) = &first.subject {
        e = e.with_context("subject", subject.chars().take(200).collect::<String>());
    }
    e
}

/// The version of `tag` when it is `<slug>/v<version>` (the version:
/// 1..=64 of `[0-9A-Za-z.-]`; the manifest decides whether it is semver).
pub fn version_of_tag<'t>(slug: &str, tag: &'t str) -> Option<&'t str> {
    let version = tag.strip_prefix(slug)?.strip_prefix("/v")?;
    let ok = !version.is_empty()
        && version.len() <= 64
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    ok.then_some(version)
}

/// The full ref name of `tag`.
fn tag_ref(tag: &str) -> Result<RefName, ForgeError> {
    RefName::parse(&format!("refs/tags/{tag}")).map_err(|_| manifest_invalid("tag_invalid"))
}

fn caller(principal: &Principal, action: Action) -> Result<UserId, ForgeError> {
    principal
        .user()
        .ok_or_else(|| internal("proof_without_user").with_context("action", action.name()))
}

fn require(action: Action, expected: Action) -> Result<(), ForgeError> {
    if action == expected {
        return Ok(());
    }
    Err(internal("proof_action_mismatch")
        .with_context("command_action", expected.name())
        .with_context("proof_action", action.name()))
}

fn change_context(
    request: &MarketplaceRequest,
    principal: &Principal,
    user: UserId,
) -> ChangeContext {
    let actor = match principal.account_kind() {
        AccountKind::ServiceAccount => Actor::ServiceAccount(user),
        AccountKind::Human | AccountKind::Bot => Actor::User(user),
    };
    let mut ctx = ChangeContext::new(actor, request.request_id.clone()); // clone: the context owns it
    ctx.credential_id = request.credential_id;
    ctx.effective_scope = principal.scope_names();
    ctx
}

async fn bounded<T>(
    operation: &'static str,
    fut: impl std::future::Future<Output = Result<T, ForgeError>>,
) -> Result<T, ForgeError> {
    match tokio::time::timeout(PUBLISH_TIMEOUT, fut).await {
        Ok(result) => result,
        Err(_) => Err(ForgeError::new(ErrorCode::RequestTimeout)
            .with_context("operation", operation)
            .with_context("timeout_ms", PUBLISH_TIMEOUT.as_millis())),
    }
}

/// The registered key `kid` as a key ring of one (to verify what was just
/// signed), checked against the seed's public key.
fn current_key(
    keys: &[StoredSigningKey],
    kid: &str,
    public_key: &str,
) -> Result<KeyRing, ForgeError> {
    let Some(stored) = keys.iter().find(|k| k.kid == kid) else {
        return Err(key_file("key_not_registered").with_context("kid", kid.to_owned()));
    };
    if stored.revoked {
        return Err(key_file("key_revoked").with_context("kid", kid.to_owned()));
    }
    if stored.retired {
        return Err(key_file("signing_key_retired").with_context("kid", kid.to_owned()));
    }
    if stored.public_key != public_key {
        return Err(key_file("key_mismatch").with_context("kid", kid.to_owned()));
    }
    let ring = KeyRing::parse(
        serde_json::json!({ "keys": [{ "kid": kid, "public_key": public_key }] })
            .to_string()
            .as_bytes(),
    )
    .map_err(|p| problems_error(&[p]))?;
    Ok(ring)
}

impl Marketplace {
    pub fn new(db: Db, browse: Browser, config: AgentConfig) -> Self {
        Self { db, browse, config }
    }

    /// The parts project (`agent.parts_repository`), its repository and
    /// group; `None` when there is no such project in a group.
    pub async fn source(&self) -> Result<Option<PartsSource>, ForgeError> {
        let Some((project, group)) =
            db::parts_source(&self.db, &self.config.parts_repository).await?
        else {
            return Ok(None);
        };
        let Some(record) = forge_db::resolve::load_project(&self.db, project, None).await? else {
            return Ok(None);
        };
        if record.repository.state != RepositoryState::Ready {
            return Err(ForgeError::new(ErrorCode::RepositoryNotReady)
                .with_context("project_id", project.get())
                .with_context("state", record.repository.state.as_str()));
        }
        Ok(Some(PartsSource {
            project,
            repo: record.repository.id,
            group,
        }))
    }

    /// The caller's role on the publishing group and how the group is seen
    /// (`None`: no such group).
    async fn group_facts(
        &self,
        group: GroupId,
        viewer: Option<UserId>,
    ) -> Result<Option<forge_core::GroupRecord>, ForgeError> {
        forge_db::groups::load(&self.db, group, viewer).await
    }

    /// Read `slug`'s part directory at `commit` and lint it (`Err`: the
    /// lint's problems). Bounded before anything is read: at most
    /// [`MAX_ENTRIES`] entries and [`MAX_PART_BYTES`] (plus `part.toml`).
    async fn read_part(
        &self,
        repo: RepoId,
        commit: &str,
        slug: &str,
    ) -> Result<(Result<Part, Vec<Problem>>, Vec<(String, &'static str)>), ForgeError> {
        let dir = RepoPath::parse(slug).map_err(|_| manifest_invalid("slug_invalid"))?;
        let listing = self
            .browse
            .part_entries(repo, commit, &dir, MAX_ENTRIES)
            .await?;
        if listing.truncated {
            return Err(manifest_invalid("too_many_files"));
        }
        let total: u64 = listing.entries.iter().filter_map(|e| e.size).sum();
        if total > MAX_PART_BYTES as u64 + MANIFEST_ALLOWANCE {
            return Err(manifest_invalid("too_large").with_context("bytes", total));
        }
        let blobs: Vec<ListedBlob> = listing
            .entries
            .iter()
            .filter(|e| e.kind == GitKind::Blob)
            .map(|e| ListedBlob {
                path: e.path.clone(), // clone: the listing stays for the modes
                oid: e.oid.clone(),   // clone: as path
                size: e.size.unwrap_or(0),
            })
            .collect();
        let mut contents = self.browse.read_blobs(repo, &blobs).await?.into_iter();
        let mut entries = Vec::with_capacity(listing.entries.len());
        let mut modes = Vec::with_capacity(listing.entries.len());
        for e in &listing.entries {
            let (kind, content) = match e.kind {
                GitKind::Blob => (
                    EntryKind::File,
                    contents
                        .next()
                        .ok_or_else(|| internal("blob_count_mismatch"))?,
                ),
                GitKind::Symlink => (EntryKind::Symlink, Vec::new()),
                GitKind::Submodule | GitKind::Tree => (EntryKind::Other, Vec::new()),
            };
            let mode = if e.mode == "100755" {
                "100755"
            } else {
                "100644"
            };
            modes.push((e.path.clone(), mode)); // clone: the entry keeps its own
            entries.push(TreeEntry {
                path: e.path.clone(), // clone: as above
                kind,
                content,
            });
        }
        if entries.is_empty() {
            return Err(manifest_invalid("manifest_missing").with_context("subject", slug));
        }
        Ok((parse_part(&entries), modes))
    }

    /// The route's loader of `POST /api/v1/agent-parts/{slug}/versions`
    /// (module docs, step 1): `None` when there is no parts project, no
    /// group or no such tag (the route's 404).
    pub async fn load_for_publish(
        &self,
        slug: &str,
        tag: &str,
        viewer: Option<UserId>,
    ) -> Result<Option<AgentPartCtx>, ForgeError> {
        let Some(version) = version_of_tag(slug, tag) else {
            return Err(manifest_invalid("tag_invalid"));
        };
        let Some(source) = self.source().await? else {
            return Ok(None);
        };
        let Some(group) = self.group_facts(source.group, viewer).await? else {
            return Ok(None);
        };
        let name = tag_ref(tag)?;
        // Review A5 HIGH: git is read only for a caller authorize() could
        // allow (a Maintainer of the publishing group). Anyone else gets
        // the facts without git — no pusher, nothing pinned — and the
        // decision refuses them on the group or the role first.
        if !group.role.is_some_and(|r| r >= Role::Maintainer) {
            return Ok(Some(AgentPartCtx::new(
                AgentPartTarget {
                    slug: slug.to_owned(),
                    version: version.to_owned(),
                    source_project: source.project,
                    source_sha: String::new(),
                    tag_object: String::new(),
                },
                AgentPartView {
                    group_role: group.role,
                    group_sight: group.sight,
                    tag_pusher: None,
                    tag_authors: Vec::new(),
                    network: false,
                },
            )));
        }
        let Some(entry) = self.browse.find_tag(source.repo, &name).await? else {
            return Ok(None);
        };
        // A tag of a tree or a blob publishes nothing: the same 404.
        let Some(commit) = entry.commit.clone() else {
            return Ok(None);
        };
        let (pusher, other_pushers) =
            db::tag_pushers(&self.db, source.project, name.as_str(), &entry.target).await?;
        let ids = self
            .browse
            .tag_identities(source.repo, &entry.target, &commit)
            .await?;
        let mut emails = vec![ids.author_email, ids.committer_email];
        emails.extend(ids.tagger_email);
        let mut authors = db::users_by_email(&self.db, &emails).await?;
        authors.extend(other_pushers);
        authors.sort_by_key(|u| u.get());
        authors.dedup();
        // The part is not read before the decision (review A5 HIGH): its
        // `network` declaration is decided again by `publish`, on the part
        // read at the pinned commit, before anything is checked or signed.
        let network = false;
        Ok(Some(AgentPartCtx::new(
            AgentPartTarget {
                slug: slug.to_owned(),
                version: version.to_owned(),
                source_project: source.project,
                source_sha: commit,
                tag_object: entry.target,
            },
            AgentPartView {
                group_role: group.role,
                group_sight: group.sight,
                tag_pusher: pusher,
                tag_authors: authors,
                network,
            },
        )))
    }

    /// The loader of the promote and revoke routes: the stored version and
    /// the facts its publication recorded (`None`: no such version, no
    /// parts project or group).
    pub async fn load_version(
        &self,
        slug: &str,
        version: &str,
        viewer: Option<UserId>,
    ) -> Result<Option<AgentPartCtx>, ForgeError> {
        let Some(stored) = db::version(&self.db, slug, version).await? else {
            return Ok(None);
        };
        let Some(source) = self.source().await? else {
            return Ok(None);
        };
        // The part's own project decides its group (a moved parts
        // repository setting does not hand old parts to another group).
        if stored.source_project_id != source.project.get() {
            return Ok(None);
        }
        let Some(group) = self.group_facts(source.group, viewer).await? else {
            return Ok(None);
        };
        let (pusher, mut authors) = db::version_people(&self.db, stored.part_id, stored.id)
            .await?
            .unwrap_or((None, Vec::new()));
        // Review A5 LOW: the publisher does not promote alone either (the
        // two-person rule of promotion includes who published it).
        authors.push(UserId::new(stored.published_by));
        let network = stored.capabilities.iter().any(|c| c == "network");
        Ok(Some(AgentPartCtx::new(
            AgentPartTarget {
                slug: stored.slug,
                version: stored.version,
                source_project: source.project,
                source_sha: stored.source_sha,
                tag_object: String::new(),
            },
            AgentPartView {
                group_role: group.role,
                group_sight: group.sight,
                tag_pusher: pusher,
                tag_authors: authors,
                network,
            },
        )))
    }

    /// Publish the proof's pinned version (module docs, step 2).
    pub async fn publish(
        &self,
        request: &MarketplaceRequest,
        proof: &Authorized<AgentPartCtx>,
    ) -> Result<VersionDetail, ForgeError> {
        require(proof.action(), Action::AgentPartPublish)?;
        let user = caller(proof.principal(), Action::AgentPartPublish)?;
        bounded(
            "agent_part_publish",
            self.publish_inner(request, proof, user),
        )
        .await
    }

    async fn publish_inner(
        &self,
        request: &MarketplaceRequest,
        proof: &Authorized<AgentPartCtx>,
        user: UserId,
    ) -> Result<VersionDetail, ForgeError> {
        let target = proof.ctx().target();
        let view = proof.ctx().view();
        let Some(pusher) = view.tag_pusher else {
            return Err(internal("publish_without_pusher"));
        };
        let source = self
            .source()
            .await?
            .filter(|s| s.project == target.source_project)
            .ok_or_else(|| internal("parts_source_changed"))?;
        let (part, modes) = self
            .read_part(source.repo, &target.source_sha, &target.slug)
            .await?;
        let part = part.map_err(|p| problems_error(&p))?;
        let policy = ScanPolicy {
            mcp_allowed_hosts: self.config.mcp_allowed_hosts.clone(), // clone: the policy owns it
        };
        let found = scan(&part, &policy);
        if !found.is_empty() {
            return Err(problems_error(&found));
        }
        let m = &part.manifest;
        if m.slug != target.slug || m.version != target.version {
            return Err(manifest_invalid("tag_mismatch")
                .with_context("manifest_slug", m.slug.clone()) // clone: the context owns it
                .with_context("manifest_version", m.version.clone())); // clone: as above
        }
        // The decision again, with the part's own `network` declaration
        // (the loader did not read the part): an administrator's.
        let decided = AgentPartView {
            network: m.capabilities.contains(&Capability::Network),
            ..view.clone() // clone: the second decision is on its own copy
        };
        forge_auth::authorize(
            proof.principal(),
            Action::AgentPartPublish,
            &Resource::Agent(AgentResource::Part(decided)),
        )?;
        self.on_default_branch(source.repo, &target.source_sha)
            .await?;
        let pipeline = self
            .green_pipeline(source.project, &target.source_sha)
            .await?;
        let (kid, signature) = self.sign(&part).await?;
        let new = NewPartVersion {
            slug: m.slug.clone(), // clone: the row owns it
            kind: m.kind.as_str().to_owned(),
            source_project: source.project,
            version: m.version.clone(),            // clone: as slug
            source_sha: target.source_sha.clone(), // clone: as slug
            tag: format!("{}/v{}", m.slug, m.version),
            tag_object: target.tag_object.clone(), // clone: as slug
            tag_pusher: pusher,
            tag_authors: view.tag_authors.clone(), // clone: as slug
            manifest: manifest_json(&part),
            digest: part.digest.to_string(),
            signature,
            key_id: kid,
            capabilities: m
                .capabilities
                .iter()
                .map(|c| c.as_str().to_owned())
                .collect(),
            published_by: user,
            ci_pipeline_id: pipeline,
            files: part
                .files
                .iter()
                .map(|f| NewPartFile {
                    path: f.path.clone(), // clone: the row owns it
                    mode: modes
                        .iter()
                        .find(|(p, _)| *p == f.path)
                        .map_or("100644", |(_, m)| *m),
                    sha256: hex(&f.sha256),
                    content: f.content.clone(), // clone: as path
                })
                .collect(),
        };
        let ctx = change_context(request, proof.principal(), user);
        db::publish_version(&self.db, &ctx, &new).await?;
        tracing::info!(
            target: "forge_marketplace",
            event = "forge.agent.part_published",
            request_id = %request.request_id,
            slug = %new.slug,
            version = %new.version,
            kind = %new.kind,
            source_sha = %new.source_sha,
            digest = %new.digest,
            key_id = %new.key_id,
            ci_pipeline_id = new.ci_pipeline_id,
            files = new.files.len(),
            published_by = user.get(),
            "agent part version published (beta)"
        );
        self.version(&new.slug, &new.version)
            .await?
            .ok_or_else(|| internal("published_version_missing"))
    }

    /// The commit is reachable from the default branch's tip.
    async fn on_default_branch(&self, repo: RepoId, sha: &str) -> Result<(), ForgeError> {
        let refs = self.browse.refs(repo).await?;
        let Some(branch) = refs.default_branch.clone() else {
            return Err(checks_not_green("not_on_default_branch").with_context("detail", "no_head"));
        };
        let name = RefName::parse(&format!("refs/heads/{branch}"))?;
        let tip = match self.browse.find_ref(repo, &name).await? {
            RefLookup::Commit(rev) => rev.commit,
            RefLookup::Missing | RefLookup::NotACommit => {
                return Err(checks_not_green("not_on_default_branch")
                    .with_context("detail", "no_default_branch"))
            }
        };
        let commit = ObjectId::parse(sha).map_err(|_| internal("pinned_sha_invalid"))?;
        let tip = ObjectId::parse(&tip).map_err(|_| internal("tip_sha_invalid"))?;
        match self.browse.merge_base(repo, &commit, &tip).await? {
            Some(base) if base == commit => Ok(()),
            _ => Err(checks_not_green("not_on_default_branch").with_context("sha", sha.to_owned())),
        }
    }

    /// The newest pipeline on the commit succeeded: its id.
    async fn green_pipeline(&self, project: ProjectId, sha: &str) -> Result<i64, ForgeError> {
        match db::newest_pipeline_at(&self.db, project, sha).await? {
            None => Err(checks_not_green("pipeline_missing").with_context("sha", sha.to_owned())),
            Some((id, status)) if status == "succeeded" => Ok(id),
            Some((id, status)) => Err(checks_not_green("pipeline_not_green")
                .with_context("pipeline_id", id)
                .with_context("status", status)),
        }
    }

    /// Sign the part's digest with the file's highest kid (registered,
    /// neither retired nor revoked, the same public key), then verify the
    /// signature with the registered key: `(kid, signature)`.
    async fn sign(&self, part: &Part) -> Result<(String, String), ForgeError> {
        let Some(path) = self.config.signing_keys_file.as_deref() else {
            return Err(key_file("not_configured"));
        };
        // The marketplace's publish step is the one caller (ADR-0032
        // Amendment A5); the seed is dropped (zeroized) on return.
        #[allow(clippy::disallowed_methods)]
        let signer = forge_core::agent_signing::read_signing_seed(path)?;
        let kid = signer.kid().to_string();
        let public_key =
            forge_agent::public_key_from_seed(signer.seed()).map_err(|p| problems_error(&[p]))?;
        let keys = db::signing_keys(&self.db).await?;
        let ring = current_key(&keys, &kid, &public_key)?;
        // The one signing step of agent parts (clippy.toml).
        #[allow(clippy::disallowed_methods)]
        let signature = forge_agent::sign_digest(signer.seed(), &part.digest)
            .map_err(|p| problems_error(&[p]))?;
        drop(signer);
        forge_agent::verify_signature(&ring, &kid, &part.digest, &signature)
            .map_err(|p| problems_error(&[p]))?;
        Ok((kid, signature))
    }

    /// Promote the proof's version from beta to stable (`false`: it was
    /// stable already).
    pub async fn promote(
        &self,
        request: &MarketplaceRequest,
        proof: &Authorized<AgentPartCtx>,
    ) -> Result<(bool, VersionDetail), ForgeError> {
        require(proof.action(), Action::AgentPartPromote)?;
        let user = caller(proof.principal(), Action::AgentPartPromote)?;
        let target = proof.ctx().target();
        let ctx = change_context(request, proof.principal(), user);
        let changed = db::set_channel_stable(&self.db, &ctx, &target.slug, &target.version).await?;
        tracing::info!(
            target: "forge_marketplace",
            event = "forge.agent.part_promoted",
            request_id = %request.request_id,
            slug = %target.slug,
            version = %target.version,
            changed,
            by = user.get(),
            "agent part version on the stable channel"
        );
        let detail = self
            .version(&target.slug, &target.version)
            .await?
            .ok_or_else(|| internal("promoted_version_missing"))?;
        Ok((changed, detail))
    }

    /// Revoke the proof's version for `reason` (1..=500 characters after
    /// trimming; `FG-WEB-0006 param=reason` otherwise). Final.
    pub async fn revoke(
        &self,
        request: &MarketplaceRequest,
        proof: &Authorized<AgentPartCtx>,
        reason: &str,
    ) -> Result<VersionDetail, ForgeError> {
        require(proof.action(), Action::AgentPartRevoke)?;
        let user = caller(proof.principal(), Action::AgentPartRevoke)?;
        let reason = reason.trim();
        let length = reason.chars().count();
        if !(1..=500).contains(&length) || reason.chars().any(char::is_control) {
            return Err(ForgeError::new(ErrorCode::RequestInvalid)
                .with_context("param", "reason")
                .with_context("reason", "length_or_characters"));
        }
        let target = proof.ctx().target();
        let ctx = change_context(request, proof.principal(), user);
        db::revoke_version(&self.db, &ctx, &target.slug, &target.version, user, reason).await?;
        tracing::warn!(
            target: "forge_marketplace",
            event = "forge.agent.part_revoked",
            request_id = %request.request_id,
            slug = %target.slug,
            version = %target.version,
            by = user.get(),
            "agent part version revoked"
        );
        self.version(&target.slug, &target.version)
            .await?
            .ok_or_else(|| internal("revoked_version_missing"))
    }

    /// The catalog (`AgentPartList`).
    pub async fn list(
        &self,
        kind: Option<&str>,
        channel: Option<&str>,
        q: Option<&str>,
    ) -> Result<Vec<PartSummary>, ForgeError> {
        db::list_parts(&self.db, kind, channel, q).await
    }

    /// Every version of a part, newest first (`None`: no such part).
    pub async fn part(&self, slug: &str) -> Result<Option<Vec<StoredVersion>>, ForgeError> {
        db::part(&self.db, slug).await
    }

    /// One version with its files (`None`: no such version).
    pub async fn version(
        &self,
        slug: &str,
        version: &str,
    ) -> Result<Option<VersionDetail>, ForgeError> {
        let Some(stored) = db::version(&self.db, slug, version).await? else {
            return Ok(None);
        };
        let files = db::version_files(&self.db, stored.id).await?;
        Ok(Some(VersionDetail {
            version: stored,
            files,
        }))
    }

    /// The registered public keys with their times (`AgentKeyRead`).
    pub async fn keys(&self) -> Result<Vec<db::ListedSigningKey>, ForgeError> {
        db::signing_key_list(&self.db).await
    }
}

/// The manifest as stored (`agent_part_versions.manifest`).
fn manifest_json(part: &Part) -> serde_json::Value {
    let m = &part.manifest;
    serde_json::json!({
        "slug": m.slug,
        "kind": m.kind.as_str(),
        "version": m.version,
        "requires": m.requires,
        "capabilities": m.capabilities.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
        "files": m.files,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        s.push_str(&format!("{b:02x}"));
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_is_read_from_the_tag_of_the_slug_only() {
        assert_eq!(
            version_of_tag("guard-hook", "guard-hook/v1.2.3"),
            Some("1.2.3")
        );
        assert_eq!(
            version_of_tag("guard-hook", "guard-hook/v1.0.0-beta.1"),
            Some("1.0.0-beta.1")
        );
        assert_eq!(version_of_tag("guard-hook", "other/v1.0.0"), None);
        assert_eq!(version_of_tag("guard-hook", "guard-hook/1.0.0"), None);
        assert_eq!(version_of_tag("guard-hook", "guard-hook/v"), None);
        assert_eq!(version_of_tag("guard-hook", "guard-hook/v1.0/../x"), None);
        assert_eq!(version_of_tag("guard", "guard-hook/v1.0.0"), None);
    }

    #[test]
    fn the_signing_key_must_be_registered_current_and_the_same() {
        let pk = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
        let key = |retired, revoked, public_key: &str| StoredSigningKey {
            kid: "1".to_owned(),
            public_key: public_key.to_owned(),
            retired,
            revoked,
        };
        let reason = |e: ForgeError| {
            e.context()
                .iter()
                .find(|(k, _)| *k == "reason")
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(
            reason(current_key(&[], "1", pk).unwrap_err()),
            "key_not_registered"
        );
        assert_eq!(
            reason(current_key(&[key(false, true, pk)], "1", pk).unwrap_err()),
            "key_revoked"
        );
        assert_eq!(
            reason(current_key(&[key(true, false, pk)], "1", pk).unwrap_err()),
            "signing_key_retired"
        );
        let other = "AQECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
        assert_eq!(
            reason(current_key(&[key(false, false, other)], "1", pk).unwrap_err()),
            "key_mismatch"
        );
        assert!(current_key(&[key(false, false, pk)], "1", pk).is_ok());
    }
}
