//! `pfs-ai-bundle` releases (plan 17 P17-7 §1.8–§1.11, ADR-0031 Amendment
//! P17-7): a person's tag `assets/v<yyyy>.<ww>.<n>` of a project whose
//! `ai_assets.enabled` is on becomes a signed manifest, deployed through
//! the three approved stages and rolled back to the release last approved
//! before.
//!
//! **Making a release** (`AiAssetReleaseCreate`, [`Deploy::create_release`]):
//!
//! 1. The tag is read with git (through [`BundleSource`]) and the commit it
//!    names **now is pinned**: everything below reads that commit, and the
//!    release row keeps it. A tag moved afterwards changes nothing; making
//!    the version (or another version of the commit) again is
//!    `FG-AIASSET-0008 already_released`.
//! 2. A protected branch must hold the commit ([`CommitSource`], as for a
//!    deployment), and its `forge/ai-eval` check — the release candidate's
//!    bench (plan 17 §1.6) — must have concluded `success`
//!    (`bench_missing`, `bench_not_green`). Only then is anything signed.
//! 3. The files of the bundle directories ([`forge_agent::bundle::BUNDLE_DIRS`]:
//!    never `evals/`, `thresholds/` or `manifest/`) are read at the pinned
//!    commit, bounded, and linted with the same code as `gstn ai-asset
//!    lint` (forge-doc) plus `release_problems` (`lint_failed`).
//! 4. The manifest (files and their SHA-256, the models, the bench) is
//!    signed with the agent signing key (the key file's highest kid,
//!    registered and current: `FG-AGENT-0014` otherwise) in the bundle
//!    domain, verified at once from its JSON with the registered public
//!    key, and written with its audit row in one transaction, decided
//!    again on the locked project with `ai_assets.enabled` read under the
//!    lock.
//!
//! **Deploying** ([`Deploy::request_release`], `DeployCreate`): an ordinary
//! deployment of the release's commit, plus the release checks of
//! `forge_db::deploy::request_bundle_decided` (the environment requires a
//! person's approval; shadow → canary → prod). Approval is `DeployApprove`
//! as for every deployment: people only, a web session, Maintainer or a
//! named approver (ADR-0038 §3), never anyone behind the commit.
//!
//! **Rolling back** ([`Deploy::rollback_release`], `DeployCreate`): a new
//! deployment request of the release last approved before the one that
//! runs (`forge_db::deploy::ai_bundle::rollback_target`), approved like any
//! other. The worker's automatic rollback of a failed release goes only to
//! a release that succeeded there with an approval.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use forge_agent::bundle::{
    version_of_tag, BundleBench, BundleFile, BundleManifest, BUNDLE_DIRS, MAX_BUNDLE_BYTES,
    MAX_BUNDLE_FILES,
};
use forge_agent::KeyRing;
use forge_auth::{Action, Authorized, ProjectCtx};
use forge_core::checks::{CheckConclusion, CheckStatus, AI_EVAL_CHECK};
use forge_core::deploy::DeployTier;
use forge_core::{ObjectId, ProjectId, RefName, RepositoryState};
use forge_db::deploy::ai_bundle::{self as db, NewRelease, RollbackTarget};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{Browser, EntryKind, ListedBlob, RepoPath};

pub use forge_db::deploy::ai_bundle::StoredRelease;

use crate::{caller, change_context, reconfirm, require, ChangeRequest, Deploy, StoredDeployment};

/// One bound for the whole release step's reads, signature and write.
pub const RELEASE_TIMEOUT: Duration = Duration::from_secs(60);

/// A tag as git has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagFacts {
    /// The tag's object (an annotated tag's own id, or the commit).
    pub object: String,
    /// The commit it peels to (`None`: a tag of a tree or a blob).
    pub commit: Option<String>,
}

/// One file of a bundle directory at the pinned commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// What [`BundleSource::tag`] returns.
pub type TagFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<TagFacts>, ForgeError>> + Send + 'a>>;
/// What [`BundleSource::files`] returns.
pub type FilesFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<TreeFile>, ForgeError>> + Send + 'a>>;

/// Reads a release's tag and files (git in `forge serve`; a fake in tests).
pub trait BundleSource: Send + Sync {
    /// The tag `name` (`assets/…`, without `refs/tags/`; `None`: no such tag).
    fn tag<'a>(&'a self, project: ProjectId, name: &'a str) -> TagFuture<'a>;

    /// Every regular file under the bundle directories at `commit`, bounded
    /// by [`MAX_BUNDLE_FILES`] and [`MAX_BUNDLE_BYTES`] before anything is
    /// read (`FG-AIASSET-0008`: `too_many_files`, `too_large`,
    /// `not_a_regular_file` for a symlink or a submodule).
    fn files<'a>(&'a self, project: ProjectId, commit: &'a str) -> FilesFuture<'a>;
}

pub(crate) fn refused(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::BundleReleaseRefused).with_context("reason", reason)
}

fn deploy_refused(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::BundleDeployRefused).with_context("reason", reason)
}

fn internal(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", reason)
}

fn agent_problem(p: &forge_agent::Problem) -> ForgeError {
    let mut e = ForgeError::new(p.code).with_context("reason", p.reason);
    if let Some(s) = &p.subject {
        e = e.with_context("subject", s.chars().take(200).collect::<String>());
    }
    e
}

/// The real source: the project's repository through the browse API
/// (argv, the repository path from the id, the browse timeout).
#[derive(Clone)]
pub struct GitBundles {
    db: Db,
    browse: Browser,
}

impl GitBundles {
    pub fn new(db: Db, browse: Browser) -> Self {
        Self { db, browse }
    }

    async fn repo(&self, project: ProjectId) -> Result<Option<forge_core::RepoId>, ForgeError> {
        let Some(record) = forge_db::resolve::load_project(&self.db, project, None).await? else {
            return Ok(None);
        };
        if record.repository.state != RepositoryState::Ready {
            return Err(ForgeError::new(ErrorCode::RepositoryNotReady)
                .with_context("project_id", project.get())
                .with_context("state", record.repository.state.as_str()));
        }
        Ok(Some(record.repository.id))
    }

    async fn read_tag(
        &self,
        project: ProjectId,
        name: &str,
    ) -> Result<Option<TagFacts>, ForgeError> {
        let Some(repo) = self.repo(project).await? else {
            return Ok(None);
        };
        let full =
            RefName::parse(&format!("refs/tags/{name}")).map_err(|_| refused("tag_invalid"))?;
        Ok(self.browse.find_tag(repo, &full).await?.map(|e| TagFacts {
            object: e.target,
            commit: e.commit,
        }))
    }

    async fn read_files(
        &self,
        project: ProjectId,
        commit: &str,
    ) -> Result<Vec<TreeFile>, ForgeError> {
        let Some(repo) = self.repo(project).await? else {
            return Err(internal("bundle_project_missing"));
        };
        let mut listed: Vec<(String, ListedBlob)> = Vec::new();
        let mut total: u64 = 0;
        for dir in BUNDLE_DIRS {
            let path = RepoPath::parse(dir).map_err(|_| internal("bundle_dir_invalid"))?;
            let left = MAX_BUNDLE_FILES.saturating_sub(listed.len()) + 1;
            let listing = self.browse.part_entries(repo, commit, &path, left).await?;
            if listing.truncated {
                return Err(refused("too_many_files"));
            }
            for e in listing.entries {
                let full = format!("{dir}/{}", e.path);
                if e.kind != EntryKind::Blob {
                    return Err(refused("not_a_regular_file").with_context("subject", full));
                }
                let size = e.size.unwrap_or(0);
                total = total.saturating_add(size);
                listed.push((
                    full.clone(), // clone: the listed blob keeps its own path
                    ListedBlob {
                        path: full,
                        oid: e.oid,
                        size,
                    },
                ));
            }
            if listed.len() > MAX_BUNDLE_FILES {
                return Err(refused("too_many_files"));
            }
            if total > MAX_BUNDLE_BYTES {
                return Err(refused("too_large").with_context("bytes", total));
            }
        }
        let blobs: Vec<ListedBlob> = listed.iter().map(|(_, b)| b.clone()).collect();
        let contents = self.browse.read_blobs(repo, &blobs).await?;
        if contents.len() != listed.len() {
            return Err(internal("blob_count_mismatch"));
        }
        Ok(listed
            .into_iter()
            .zip(contents)
            .map(|((path, _), bytes)| TreeFile { path, bytes })
            .collect())
    }
}

impl BundleSource for GitBundles {
    fn tag<'a>(&'a self, project: ProjectId, name: &'a str) -> TagFuture<'a> {
        Box::pin(self.read_tag(project, name))
    }

    fn files<'a>(&'a self, project: ProjectId, commit: &'a str) -> FilesFuture<'a> {
        Box::pin(self.read_files(project, commit))
    }
}

/// Every `name:tag@sha256:<hex>` string of `models/models.toml` (the lint
/// already refused anything else), sorted and unique.
fn models_of(files: &[TreeFile]) -> Vec<String> {
    fn collect(v: &toml::Value, out: &mut Vec<String>) {
        match v {
            toml::Value::String(s) => {
                if forge_doc::ai_asset::model_ref_problem(s).is_none() {
                    out.push(s.clone()); // clone: the list owns it
                }
            }
            toml::Value::Array(a) => a.iter().for_each(|x| collect(x, out)),
            toml::Value::Table(t) => t.values().for_each(|x| collect(x, out)),
            toml::Value::Integer(_)
            | toml::Value::Float(_)
            | toml::Value::Boolean(_)
            | toml::Value::Datetime(_) => {}
        }
    }
    let mut out = Vec::new();
    for f in files.iter().filter(|f| f.path == "models/models.toml") {
        if let Some(table) = std::str::from_utf8(&f.bytes)
            .ok()
            .and_then(|t| t.parse::<toml::Table>().ok())
        {
            collect(&toml::Value::Table(table), &mut out);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The lint of what the bundle ships: forge-doc's AI asset lint and the
/// release list check; the first problem as `lint_failed`.
fn lint(files: &[TreeFile]) -> Result<(), ForgeError> {
    // Review P17-7: only the asset kinds the lint knows ship (an unknown
    // file — a script, a nested file — would be signed unchecked); an
    // empty directory's `.gitkeep` is the one exception.
    if let Some(f) = files.iter().find(|f| {
        forge_doc::ai_asset::kind_of(&f.path).is_none()
            && f.path.rsplit('/').next() != Some(".gitkeep")
    }) {
        return Err(refused("lint_failed")
            .with_context("rule", "not_an_asset_file")
            .with_context("path", f.path.chars().take(200).collect::<String>()));
    }
    let assets: Vec<forge_doc::ai_asset::AssetFile<'_>> = files
        .iter()
        .map(|f| forge_doc::ai_asset::AssetFile {
            path: &f.path,
            bytes: &f.bytes,
        })
        .collect();
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    let mut problems = forge_doc::ai_asset::lint(&assets);
    problems.extend(forge_doc::ai_asset::release_problems(&paths));
    let Some(first) = problems.first() else {
        return Ok(());
    };
    let mut e = refused("lint_failed")
        .with_context("rule", first.rule)
        .with_context("path", first.path.chars().take(200).collect::<String>())
        .with_context("problems", problems.len());
    if let Some(s) = &first.subject {
        e = e.with_context("subject", s.clone()); // clone: the context owns it
    }
    Err(e)
}

impl Deploy {
    fn bundles(&self) -> Result<&dyn BundleSource, ForgeError> {
        self.bundles
            .as_deref()
            .ok_or_else(|| internal("ai_bundles_unconfigured"))
    }

    /// `GET …/ai-asset-releases` (`DeployRead`): the newest releases.
    pub async fn releases(
        &self,
        proof: &Authorized<ProjectCtx>,
    ) -> Result<Vec<StoredRelease>, ForgeError> {
        require(proof.action(), Action::DeployRead)?;
        db::list_releases(&self.db, *proof.id()).await
    }

    /// `GET …/ai-asset-releases/{version}` (`DeployRead`; `None`: no such
    /// release).
    pub async fn release(
        &self,
        proof: &Authorized<ProjectCtx>,
        version: &str,
    ) -> Result<Option<StoredRelease>, ForgeError> {
        require(proof.action(), Action::DeployRead)?;
        db::release(&self.db, *proof.id(), version).await
    }

    /// `POST …/ai-asset-releases` (`AiAssetReleaseCreate`): make the
    /// release of `tag` (module docs).
    pub async fn create_release(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<ProjectCtx>,
        tag: &str,
    ) -> Result<StoredRelease, ForgeError> {
        require(proof.action(), Action::AiAssetReleaseCreate)?;
        let project = *proof.id();
        let outcome = match tokio::time::timeout(
            RELEASE_TIMEOUT,
            self.create_release_inner(request, proof, tag),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(ForgeError::new(ErrorCode::RequestTimeout)
                .with_context("operation", "ai_asset_release")
                .with_context("timeout_ms", RELEASE_TIMEOUT.as_millis())),
        };
        if let Err(e) = &outcome {
            if e.code() == ErrorCode::BundleReleaseRefused {
                let reason = e
                    .context()
                    .iter()
                    .find(|(k, _)| *k == "reason")
                    .map(|(_, v)| v.clone()) // clone: logged below
                    .unwrap_or_default();
                tracing::info!(
                    target: "forge_deploy",
                    event = "forge.deploy.bundle_refused",
                    error_code = ErrorCode::BundleReleaseRefused.id(),
                    request_id = %request.request_id,
                    project_id = project.get(),
                    tag = %tag.chars().take(80).collect::<String>(),
                    reason = %reason,
                    "ai asset release refused"
                );
            }
        }
        outcome
    }

    async fn create_release_inner(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<ProjectCtx>,
        tag: &str,
    ) -> Result<StoredRelease, ForgeError> {
        let user = caller(proof.principal(), Action::AiAssetReleaseCreate)?;
        let project = *proof.id();
        let Some(version) = version_of_tag(tag) else {
            return Err(refused("tag_invalid"));
        };
        // A fast answer before git is read; decided again under the lock.
        if !forge_db::ai_assets::setting(&self.db, project)
            .await?
            .enabled
        {
            return Err(refused("ai_assets_disabled"));
        }
        let source = self.bundles()?;
        let Some(facts) = source.tag(project, tag).await? else {
            return Err(refused("tag_not_found"));
        };
        let Some(commit) = facts.commit else {
            return Err(refused("tag_not_a_commit"));
        };
        // Pinned: everything below reads this commit.
        let commit = ObjectId::parse(&commit).map_err(|_| internal("tag_commit_unparsable"))?;
        let tag_object =
            ObjectId::parse(&facts.object).map_err(|_| internal("tag_object_unparsable"))?;
        if db::release(&self.db, project, version).await?.is_some()
            || db::release_at(&self.db, project, commit.as_str())
                .await?
                .is_some()
        {
            return Err(refused("already_released"));
        }
        let held = self.commits.facts(project, &commit).await?;
        if !held.is_some_and(|f| f.protected_branch.is_some()) {
            return Err(refused("not_on_protected_branch"));
        }
        let bench = forge_db::checks::check_runs(&self.db, project, &commit)
            .await?
            .into_iter()
            .find(|r| r.name.as_str() == AI_EVAL_CHECK)
            .ok_or_else(|| refused("bench_missing"))?;
        let conclusion = bench.conclusion.map_or("none", CheckConclusion::as_str);
        if bench.status != CheckStatus::Completed
            || bench.conclusion != Some(CheckConclusion::Success)
        {
            return Err(refused("bench_not_green")
                .with_context("status", bench.status.as_str())
                .with_context("conclusion", conclusion));
        }
        let files = source.files(project, commit.as_str()).await?;
        lint(&files)?;
        let record = forge_db::resolve::load_project(&self.db, project, None)
            .await?
            .ok_or_else(|| internal("release_project_missing"))?;
        let manifest = BundleManifest {
            project: record.path.full_path(),
            version: version.to_owned(),
            commit: commit.as_str().to_owned(),
            models: models_of(&files),
            files: files
                .iter()
                .map(|f| BundleFile {
                    path: f.path.clone(), // clone: the manifest owns it
                    sha256: forge_agent::sha256(&f.bytes),
                    size: f.bytes.len() as u64,
                })
                .collect(),
            bench: BundleBench {
                check: AI_EVAL_CHECK.to_owned(),
                conclusion: conclusion.to_owned(),
                summary: bench.summary.clone(), // clone: the manifest owns it
            },
        }
        .canonical()
        .map_err(|p| agent_problem(&p))?;
        let json = manifest.to_json();
        let digest = manifest.digest();
        let (kid, signature, ring) = self.sign(&json, &digest).await?;
        let new = NewRelease {
            version: version.to_owned(),
            tag_object: tag_object.as_str().to_owned(),
            commit_sha: commit.as_str().to_owned(),
            manifest: json,
            digest: digest.to_string(),
            key_id: kid,
            signature,
            bench_check_run_id: bench.id,
            files: i32::try_from(manifest.files.len()).map_err(|_| refused("too_many_files"))?,
        };
        let ctx = change_context(request, proof.principal(), user);
        let stored = db::create_bundle_decided(
            &self.db,
            &ctx,
            project,
            &new,
            |facts| reconfirm(proof, facts),
            // Review P17-7: the manifest as stored (jsonb) verifies, or
            // nothing is committed.
            |stored| {
                forge_agent::bundle::verify_bundle(
                    stored.manifest.to_string().as_bytes(),
                    &stored.digest,
                    &stored.key_id,
                    &stored.signature,
                    &ring,
                )
                .map(drop)
                .map_err(|p| agent_problem(&p))
            },
        )
        .await?;
        tracing::info!(
            target: "forge_deploy",
            event = "forge.deploy.bundle_released",
            request_id = %request.request_id,
            project_id = project.get(),
            release_id = stored.id,
            version = %stored.version,
            sha = %stored.commit_sha,
            digest = %stored.digest,
            key_id = %stored.key_id,
            files = stored.files,
            bench_check_run_id = stored.bench_check_run_id,
            user_id = user.get(),
            credential_id = request.credential_id,
            "ai asset release made (pfs-ai-bundle)"
        );
        Ok(stored)
    }

    /// Sign `digest` with the agent signing key file's highest kid
    /// (registered, neither retired nor revoked, the same public key),
    /// then verify the release from its JSON with the registered key.
    async fn sign(
        &self,
        manifest: &serde_json::Value,
        digest: &forge_agent::Digest,
    ) -> Result<(String, String, KeyRing), ForgeError> {
        let key_file = |reason: &'static str| forge_core::agent_signing::key_file_invalid(reason);
        let Some(path) = self.signing_keys_file.as_deref() else {
            return Err(key_file("not_configured"));
        };
        // The release step is the one caller in forge-deploy, after the
        // authorized release decision and the bench (ADR-0031 Amendment
        // P17-7); the seed is dropped (zeroized) on return.
        #[allow(clippy::disallowed_methods)]
        let signer = forge_core::agent_signing::read_signing_seed(path)?;
        let kid = signer.kid().to_string();
        let public_key =
            forge_agent::public_key_from_seed(signer.seed()).map_err(|p| agent_problem(&p))?;
        let keys = forge_db::agent_marketplace::signing_keys(&self.db).await?;
        let Some(stored) = keys.iter().find(|k| k.kid == kid) else {
            return Err(key_file("key_not_registered").with_context("kid", kid));
        };
        if stored.revoked {
            return Err(key_file("key_revoked").with_context("kid", kid));
        }
        if stored.retired {
            return Err(key_file("signing_key_retired").with_context("kid", kid));
        }
        if stored.public_key != public_key {
            return Err(key_file("key_mismatch").with_context("kid", kid));
        }
        // The one signing step of pfs-ai-bundle releases (clippy.toml).
        #[allow(clippy::disallowed_methods)]
        let signature = forge_agent::bundle::sign_bundle(signer.seed(), digest)
            .map_err(|p| agent_problem(&p))?;
        drop(signer);
        let ring = KeyRing::parse(
            serde_json::json!({ "keys": [{ "kid": kid, "public_key": public_key }] })
                .to_string()
                .as_bytes(),
        )
        .map_err(|p| agent_problem(&p))?;
        forge_agent::bundle::verify_bundle(
            manifest.to_string().as_bytes(),
            &digest.to_string(),
            &kid,
            &signature,
            &ring,
        )
        .map_err(|p| agent_problem(&p))?;
        Ok((kid, signature, ring))
    }

    /// `POST …/ai-asset-releases/{version}/deployments` (`DeployCreate`):
    /// request the release on `tier` (module docs).
    pub async fn request_release(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<ProjectCtx>,
        tier: DeployTier,
        version: &str,
    ) -> Result<StoredDeployment, ForgeError> {
        require(proof.action(), Action::DeployCreate)?;
        let Some(release) = db::release(&self.db, *proof.id(), version).await? else {
            return Err(self.deploy_refusal(request, proof, tier, version, "release_not_found"));
        };
        self.request_release_sha(request, proof, tier, &release, false)
            .await
    }

    /// `POST …/ai-asset-releases/rollback` (`DeployCreate`): request the
    /// release last approved before the one `tier` runs (module docs).
    pub async fn rollback_release(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<ProjectCtx>,
        tier: DeployTier,
    ) -> Result<(StoredRelease, StoredDeployment), ForgeError> {
        require(proof.action(), Action::DeployCreate)?;
        let target = match db::rollback_target(&self.db, *proof.id(), tier).await? {
            RollbackTarget::To { current, target } => {
                tracing::info!(
                    target: "forge_deploy",
                    event = "forge.deploy.bundle_rollback",
                    request_id = %request.request_id,
                    project_id = proof.id().get(),
                    environment = tier.bundle_stage(),
                    from_version = %current.version,
                    to_version = %target.version,
                    "rolling an ai asset release back to the release last approved before"
                );
                target
            }
            RollbackTarget::NothingRunning | RollbackTarget::NoEarlier { .. } => {
                return Err(self.deploy_refusal(request, proof, tier, "", "no_rollback_target"));
            }
        };
        let stored = self
            .request_release_sha(request, proof, tier, &target, true)
            .await?;
        Ok((*target, stored))
    }

    fn deploy_refusal(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<ProjectCtx>,
        tier: DeployTier,
        version: &str,
        reason: &'static str,
    ) -> ForgeError {
        tracing::info!(
            target: "forge_deploy",
            event = "forge.deploy.bundle_deploy_refused",
            error_code = ErrorCode::BundleDeployRefused.id(),
            request_id = %request.request_id,
            project_id = proof.id().get(),
            environment = tier.bundle_stage(),
            version = %version.chars().take(32).collect::<String>(),
            reason,
            "ai asset release deployment refused"
        );
        deploy_refused(reason).with_context("environment", tier.bundle_stage())
    }

    async fn request_release_sha(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<ProjectCtx>,
        tier: DeployTier,
        release: &StoredRelease,
        rollback: bool,
    ) -> Result<StoredDeployment, ForgeError> {
        let user = caller(proof.principal(), Action::DeployCreate)?;
        let project = *proof.id();
        let sha = ObjectId::parse(&release.commit_sha)
            .map_err(|_| internal("stored_release_sha_invalid"))?;
        // The commit is still on a protected branch (as for any request),
        // but for a rollback: its release was checked on one when it was
        // made, signed and approved on this stage, and a branch moved during
        // an incident must not keep the last good release away (review
        // P17-7). It is approved again either way.
        let Some(facts) = self.commits.facts(project, &sha).await? else {
            return Err(ForgeError::new(ErrorCode::DeployCommitNotAllowed)
                .with_context("reason", "commit_not_found"));
        };
        if facts.protected_branch.is_none() && !rollback {
            return Err(ForgeError::new(ErrorCode::DeployCommitNotAllowed)
                .with_context("reason", "not_on_protected_branch"));
        }
        let commit = forge_db::deploy::CommitInput {
            subject: facts.subject,
            author_name: facts.author_name,
            author_email: facts.author_email,
            committer_email: facts.committer_email,
        };
        let ctx = change_context(request, proof.principal(), user);
        let stored = forge_db::deploy::request_bundle_decided(
            &self.db,
            &ctx,
            (project, tier, sha.as_str()),
            Some(&commit),
            |facts| reconfirm(proof, facts),
        )
        .await
        .inspect_err(|e| {
            if e.code() == ErrorCode::BundleDeployRefused {
                tracing::info!(
                    target: "forge_deploy",
                    event = "forge.deploy.bundle_deploy_refused",
                    error_code = ErrorCode::BundleDeployRefused.id(),
                    request_id = %request.request_id,
                    project_id = project.get(),
                    environment = tier.bundle_stage(),
                    version = %release.version,
                    "ai asset release deployment refused"
                );
            }
        })?;
        tracing::info!(
            target: "forge_deploy",
            event = "forge.deploy.bundle_requested",
            request_id = %request.request_id,
            deployment_id = stored.id,
            project_id = project.get(),
            environment = tier.bundle_stage(),
            version = %release.version,
            sha = %stored.sha,
            status = stored.status.as_str(),
            user_id = user.get(),
            "ai asset release deployment requested"
        );
        Ok(stored)
    }
}

// Plan 17 P17-7b: the withdrawal and the releases screen's overview.
#[path = "bundle_board.rs"]
mod board;
pub use board::{next_stage, ReleaseOverview, ReleaseRow, StageRow, STAGES};
pub use forge_db::deploy::ai_bundle::{ReleaseStage, StoredWithdrawal};

#[cfg(test)]
#[path = "bundle_tests.rs"]
mod tests;
