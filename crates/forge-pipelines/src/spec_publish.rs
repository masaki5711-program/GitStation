//! The specification pipeline's `spec-publish` (M4 C12, plan 04 §6, plan 03
//! §5.1): "承認後の版の公開（タグ）と spec.released イベントの発行".
//!
//! A version is published by pushing a version-shaped tag
//! (`forge_core::spec_version`: `v1.5`, `spec-v2.0`) — creating it is
//! guarded by the repository's protected-ref rules like any push. The
//! `spec.publish` job (one per created tag) then decides, with git and
//! nothing else, whether the tag publishes a specification:
//!
//! 0. (M4 C12 review) a **person** pushed it — not a service account, a
//!    bot, nor a person's agent token (`pusher_not_a_person`) — and a
//!    protection rule covers the tag, so only the people the rule allows
//!    can create one (`tag_not_protected`);
//! 1. the tag still names the pushed object and peels to a commit;
//! 2. that commit is on the **default branch** (`merge-base --is-ancestor`
//!    with its tip): it reached the trunk through review — a tag of an
//!    unmerged commit publishes nothing (`not_on_default_branch`);
//! 3. the commit holds the repository's own specification directory (the
//!    `path:` entries of `.forge/repo.yml`, default `spec/`); a release tag
//!    of a repository without one publishes nothing (`no_specification`).
//!
//! Then one `spec.released` event (integration class: Kanflow reads it from
//! the feed) with one audit row, actor `system`, the pusher as
//! `released_by` (`forge_db::spec_releases::record_release`), once per
//! (project, tag): a version already released is not released again. A decision
//! not to publish is logged at info with its `reason`; a failure (git,
//! database) is returned for the job to retry.

use std::time::Duration;

use forge_core::repo_config::{parse_repo_config, SpecSource, REPO_CONFIG_PATH};
use forge_core::{ObjectId, ProjectAccessLoader, RepoId, RepositoryState, RequestId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::spec_releases::{pusher_is_person, record_release, NewRelease, PublishRequest};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{Browser, RepoPath, SmallFile};
use forge_git::{Git, StorageRoot};

/// The default own specification directory (forge-checks `spec_index`).
pub const DEFAULT_SPEC_DIR: &str = "spec";
/// The ancestry question's bound.
const ANCESTRY_TIMEOUT: Duration = Duration::from_secs(20);
/// The project lookup's bound.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

/// What [`publish`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Published {
    /// The event was recorded (`false`: it already was — a retry).
    Released { version: String, recorded: bool },
    /// No release (`reason`: `project_gone`, `pusher_not_a_person`,
    /// `tag_not_protected`, `tag_gone`, `tag_moved`,
    /// `not_a_commit`, `no_default_branch`, `not_on_default_branch`,
    /// `no_specification`).
    NotReleased(&'static str),
}

/// The git handles the decision needs.
#[derive(Clone)]
pub struct SpecPublisher {
    db: Db,
    browse: Browser,
    git: Git,
    storage: StorageRoot,
}

impl std::fmt::Debug for SpecPublisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpecPublisher").finish_non_exhaustive()
    }
}

fn not_released(
    request: &PublishRequest,
    request_id: &RequestId,
    reason: &'static str,
) -> Published {
    tracing::info!(
        target: "forge_pipelines",
        event = "forge.spec.release_skipped",
        request_id = %request_id,
        project_id = request.project.get(),
        tag = request.tag.as_str(),
        reason,
        "the tag publishes no specification version"
    );
    Published::NotReleased(reason)
}

/// The own specification directories `.forge/repo.yml` names (default
/// `spec`), without a trailing `/`. A file that cannot be read as a
/// configuration counts as none stated (the default).
pub fn own_spec_dirs(config: Option<&[u8]>) -> Vec<String> {
    let stated: Vec<String> = config
        .and_then(|bytes| parse_repo_config(bytes).ok())
        .map(|c| {
            c.specs
                .iter()
                .filter_map(|e| match &e.source {
                    SpecSource::Path(p) => Some(p.as_str().trim_end_matches('/').to_owned()),
                    SpecSource::Repo { .. } => None,
                })
                .filter(|p| !p.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if stated.is_empty() {
        vec![DEFAULT_SPEC_DIR.to_owned()]
    } else {
        stated
    }
}

impl SpecPublisher {
    pub fn new(db: Db, browse: Browser, git: Git, storage: StorageRoot) -> Self {
        Self {
            db,
            browse,
            git,
            storage,
        }
    }

    async fn repo(&self, request: &PublishRequest) -> Result<Option<RepoId>, ForgeError> {
        let loaded =
            tokio::time::timeout(LOOKUP_TIMEOUT, self.db.load_project(request.project, None))
                .await
                .map_err(|_| {
                    ForgeError::new(ErrorCode::RequestTimeout)
                        .with_context("operation", "project_load")
                        .with_context("timeout_ms", LOOKUP_TIMEOUT.as_millis())
                })??;
        let Some(record) = loaded else {
            return Ok(None);
        };
        if record.repository.state != RepositoryState::Ready {
            return Err(ForgeError::new(ErrorCode::RepositoryNotReady)
                .with_context("repo_id", record.repository.id.get())
                .with_context("project_id", request.project.get())
                .with_context("state", record.repository.state.as_str()));
        }
        Ok(Some(record.repository.id))
    }

    /// Decide and record one created tag (module docs).
    pub async fn publish(
        &self,
        request: &PublishRequest,
        request_id: &RequestId,
    ) -> Result<Published, ForgeError> {
        let Some(repo) = self.repo(request).await? else {
            return Ok(not_released(request, request_id, "project_gone"));
        };
        let person = match (request.released_by, request.push_request_id.as_deref()) {
            (Some(user), Some(push)) => pusher_is_person(&self.db, user, push).await?,
            _ => false,
        };
        if !person {
            return Ok(not_released(request, request_id, "pusher_not_a_person"));
        }
        let rules = forge_db::protected_refs::rules_for(&self.db, request.project).await?;
        if rules.levels(&request.tag).is_empty() {
            return Ok(not_released(request, request_id, "tag_not_protected"));
        }
        // The worker's browser may hold the list from before the push. A
        // reader's refresh, not a ref write: nothing is announced.
        self.browse.forget_refs(repo);
        let refs = self.browse.refs(repo).await?;
        let Some(entry) = refs.refs.iter().find(|r| r.name == request.tag) else {
            return Ok(not_released(request, request_id, "tag_gone"));
        };
        if entry.target != request.object.as_str() {
            return Ok(not_released(request, request_id, "tag_moved"));
        }
        let Some(commit) = entry
            .commit
            .as_deref()
            .and_then(|c| ObjectId::parse(c).ok())
        else {
            return Ok(not_released(request, request_id, "not_a_commit"));
        };
        let Some(default) = refs.default_branch.as_deref() else {
            return Ok(not_released(request, request_id, "no_default_branch"));
        };
        let default_ref = format!("refs/heads/{default}");
        let Some(tip) = refs
            .refs
            .iter()
            .find(|r| r.name.as_str() == default_ref)
            .and_then(|r| r.commit.as_deref())
            .and_then(|c| ObjectId::parse(c).ok())
        else {
            return Ok(not_released(request, request_id, "no_default_branch"));
        };
        let merged = self
            .git
            .is_ancestor(&self.storage, repo, &commit, &tip, ANCESTRY_TIMEOUT)
            .await?;
        if !merged {
            return Ok(not_released(request, request_id, "not_on_default_branch"));
        }
        if !self.has_specification(repo, &commit).await? {
            return Ok(not_released(request, request_id, "no_specification"));
        }
        let version = request.tag.tag().unwrap_or(request.tag.as_str()).to_owned();
        let release = NewRelease {
            project: request.project,
            repository: repo,
            tag: request.tag.clone(), // clone: the release owns it
            version: version.clone(), // clone: the answer keeps it
            commit: commit.clone(),   // clone: logged below
            released_by: request.released_by,
            default_branch: default_ref,
        };
        let ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: the context owns it
        let recorded = record_release(&self.db, &ctx, &release).await?;
        tracing::info!(
            target: "forge_pipelines",
            event = "forge.spec.released",
            request_id = %request_id,
            project_id = request.project.get(),
            tag = request.tag.as_str(),
            sha = commit.as_str(),
            recorded,
            "specification version published"
        );
        Ok(Published::Released { version, recorded })
    }

    /// Whether `commit` holds one of the repository's own specification
    /// directories.
    async fn has_specification(&self, repo: RepoId, commit: &ObjectId) -> Result<bool, ForgeError> {
        let path = RepoPath::parse(REPO_CONFIG_PATH).map_err(|rule| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "repo_config_path_invalid")
                .with_context("rule", rule)
        })?;
        let max = u64::try_from(forge_core::repo_config::MAX_REPO_CONFIG_BYTES).unwrap_or(u64::MAX);
        let config = match self
            .browse
            .small_file(repo, commit.as_str(), &path, max)
            .await?
        {
            SmallFile::Found(bytes) => Some(bytes),
            SmallFile::Absent | SmallFile::NotAFile | SmallFile::TooLarge => None,
        };
        for dir in own_spec_dirs(config.as_deref()) {
            let Ok(dir_path) = RepoPath::parse(&dir) else {
                continue;
            };
            // A directory answers "not a file"; absent is absent.
            let found = self
                .browse
                .small_file(repo, commit.as_str(), &dir_path, 1)
                .await?;
            if matches!(found, SmallFile::NotAFile) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_own_specification_directories_default_to_spec() {
        assert_eq!(own_spec_dirs(None), ["spec"]);
        assert_eq!(own_spec_dirs(Some(b"not: [valid")), ["spec"]);
        assert_eq!(
            own_spec_dirs(Some(b"specs:\n  - path: docs/spec/\n  - path: req\n")),
            ["docs/spec", "req"]
        );
        assert_eq!(
            own_spec_dirs(Some(b"specs:\n  - repo: specs/common\n    track: main\n")),
            ["spec"]
        );
    }
}
