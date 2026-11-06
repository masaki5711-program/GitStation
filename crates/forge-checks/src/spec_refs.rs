//! Reading a commit's `.forge/repo.yml` and following its references (M3
//! X1, moved here from forge-web's route by X2; ADR-0023 Amendments X1,
//! X2). Shared by `GET …/repository/spec-refs` (for the reader) and the
//! `check.spec_drift` job (for every signed-in person).
//!
//! - [`read_repo_config`]: `.forge/repo.yml` at a commit, read whole (at
//!   most 64 KiB, a regular file) and parsed by `forge_core::repo_config`
//!   — absent, refused (`FG-REPO-0024`: one problem with its line and
//!   reason, **no** entry: never a partial reading) or valid.
//! - [`follow`]: one `repo:` entry. The path is looked up once (for the
//!   reader, or for no one: [`Reader::viewer`]), the project is decided
//!   again with `authorize()` ([`Reader::decide`]: the route's proof, or
//!   every signed-in person), and only then is the pinned ref resolved
//!   (`refs/tags/<version>` / `refs/heads/<track>`, through the browse
//!   API). Every way a reference cannot be followed for this reader is the
//!   same [`Followed::Unavailable`] (no existence oracle), its reason
//!   logged once here at info (`forge.spec_refs.unresolved`, with
//!   `request_id` and `error_code`); a git failure on the referenced
//!   repository is `Unavailable` too, logged at warn with its own code. A
//!   lookup failure (the database, its time limit) is returned: never an
//!   entry quietly dropped.

use std::sync::LazyLock;
use std::time::Duration;

use forge_auth::ProjectCtx;
use forge_auth::{prove_spec_ref, prove_spec_ref_for_every_person, Authorized, LoadedProject};
use forge_core::namespace_resolver::{NamespaceResolver, ProjectAccess};
use forge_core::repo_config::{
    parse_repo_config, RepoConfig, RepoConfigProblem, SpecPin, MAX_REPO_CONFIG_BYTES,
    REPO_CONFIG_PATH,
};
use forge_core::{
    LookupPath, ProjectId, RepoId, RepositoryState, RequestId, Resolution, ResolvedTarget, UserId,
};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{Browser, RefLookup, RepoPath, SmallFile};

/// `.forge/repo.yml` as a tree path, parsed once.
static REPO_CONFIG_REPO_PATH: LazyLock<Result<RepoPath, &'static str>> =
    LazyLock::new(|| RepoPath::parse(REPO_CONFIG_PATH));

/// `.forge/repo.yml` as a tree path (a constant: its parse is tested).
pub fn repo_config_path() -> Result<RepoPath, ForgeError> {
    REPO_CONFIG_REPO_PATH.clone().map_err(|rule| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "repo_config_path_invalid")
            .with_context("rule", rule)
    })
}

/// What `.forge/repo.yml` at a commit is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoConfigRead {
    Absent,
    /// Refused as a whole (`FG-REPO-0024`).
    Invalid(RepoConfigProblem),
    Valid(RepoConfig),
}

/// Read and parse `.forge/repo.yml` at `commit` of `repo` (module docs).
/// Errors: the browse API's (git, its limits), with the commit.
pub async fn read_repo_config(
    browse: &Browser,
    repo: RepoId,
    commit: &str,
) -> Result<RepoConfigRead, ForgeError> {
    let path = repo_config_path()?;
    let file = browse
        .small_file(repo, commit, &path, MAX_REPO_CONFIG_BYTES as u64)
        .await
        .map_err(|e| e.with_context("commit", commit.to_owned()))?;
    let parsed = match file {
        SmallFile::Absent => return Ok(RepoConfigRead::Absent),
        SmallFile::NotAFile => Err(RepoConfigProblem {
            line: 0,
            reason: "not_a_file",
        }),
        SmallFile::TooLarge => Err(RepoConfigProblem {
            line: 0,
            reason: "too_large",
        }),
        SmallFile::Found(bytes) => parse_repo_config(&bytes),
    };
    Ok(match parsed {
        Ok(config) => RepoConfigRead::Valid(config),
        Err(problem) => RepoConfigRead::Invalid(problem),
    })
}

/// Who a reference is followed for.
#[derive(Debug, Clone, Copy)]
pub enum Reader<'a> {
    /// The route's reader: looked up as them, decided with
    /// `prove_spec_ref` on their proof (`SpecRefRead` on the referencing
    /// project).
    Proof(&'a Authorized<ProjectCtx>),
    /// Every signed-in person (forge's own check, shown to every reader of
    /// the referencing project): looked up for no one, decided with
    /// `prove_spec_ref_for_every_person`.
    EveryPerson,
}

impl Reader<'_> {
    /// The viewer the lookup is made for.
    pub fn viewer(&self) -> Option<UserId> {
        match self {
            Self::Proof(proof) => proof.principal().user(),
            Self::EveryPerson => None,
        }
    }

    /// `authorize()` on the referenced project; the denial's code.
    fn decide(&self, referenced: LoadedProject) -> Result<(), ForgeError> {
        match self {
            Self::Proof(proof) => prove_spec_ref(proof, referenced).map(drop),
            Self::EveryPerson => prove_spec_ref_for_every_person(referenced),
        }
    }

    /// The log's `reader` field.
    fn name(&self) -> &'static str {
        match self {
            Self::Proof(_) => "caller",
            Self::EveryPerson => "every_person",
        }
    }
}

/// What following one `repo:` entry came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Followed {
    /// The reader may read it and the pinned ref names `commit`.
    Resolved {
        project: ProjectId,
        repository: RepoId,
        commit: String,
    },
    /// The reader may read it; the pinned ref is not there (or names no
    /// commit): `FG-REPO-0025`.
    VersionNotFound { project: ProjectId },
    /// Not followed for this reader, whatever the reason: `FG-REPO-0026`.
    Unavailable,
}

/// What [`follow`] needs besides the entry.
pub struct FollowCtx<'a> {
    pub resolver: &'a dyn NamespaceResolver,
    /// One lookup (`FG-WEB-0002 operation=namespace_resolve` past it).
    pub lookup_timeout: Duration,
    pub browse: &'a Browser,
    pub request_id: &'a RequestId,
    /// The referencing project (logs).
    pub project: ProjectId,
}

/// One reference: `line` (its line in `.forge/repo.yml`), the path as
/// folded for lookup and its pin, followed for `reader` (module docs).
pub async fn follow(
    ctx: &FollowCtx<'_>,
    reader: Reader<'_>,
    line: usize,
    repo: &LookupPath,
    pin: &SpecPin,
) -> Result<Followed, ForgeError> {
    let log = |reason: &'static str,
               code: ErrorCode,
               decision: Option<ErrorCode>,
               referenced: Option<i64>| {
        tracing::info!(
            target: "forge_checks",
            event = "forge.spec_refs.unresolved",
            error_code = code.id(),
            decision_error_code = decision.map(ErrorCode::id),
            request_id = %ctx.request_id,
            project_id = ctx.project.get(),
            referenced_project_id = referenced,
            reader = reader.name(),
            line,
            pin = pin.kind(),
            reason,
            "a .forge/repo.yml reference was not resolved for the reader"
        );
    };
    let unavailable = |reason, decision, referenced| {
        log(
            reason,
            ErrorCode::SpecRepositoryUnavailable,
            decision,
            referenced,
        );
        Followed::Unavailable
    };
    let resolution = match tokio::time::timeout(
        ctx.lookup_timeout,
        ctx.resolver.resolve(repo, reader.viewer()),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            return Err(ForgeError::new(ErrorCode::RequestTimeout)
                .with_context("operation", "namespace_resolve")
                .with_context("timeout_ms", ctx.lookup_timeout.as_millis()))
        }
    };
    let resolved = match resolution {
        Resolution::Absent => return Ok(unavailable("absent", None, None)),
        Resolution::Deleted { .. } => return Ok(unavailable("tombstone_deleted", None, None)),
        Resolution::Moved { .. } => return Ok(unavailable("moved", None, None)),
        Resolution::Live(resolved) => resolved,
    };
    let (id, access, repository) = match resolved.target {
        ResolvedTarget::User(_) | ResolvedTarget::Group { .. } => {
            return Ok(unavailable("not_a_project", None, None))
        }
        ResolvedTarget::Project {
            id,
            access,
            repository,
        } => (id, access, repository),
    };
    if let Err(denied) = reader.decide(loaded(id, access)) {
        return Ok(unavailable("denied", Some(denied.code()), None));
    }
    let project = id;
    if repository.state != RepositoryState::Ready {
        return Ok(unavailable("not_ready", None, Some(project.get())));
    }
    Ok(
        match ctx.browse.find_ref(repository.id, pin.ref_name()).await {
            Ok(RefLookup::Commit(found)) => Followed::Resolved {
                project,
                repository: repository.id,
                commit: found.commit,
            },
            Ok(RefLookup::Missing) => {
                log(
                    "ref_not_found",
                    ErrorCode::SpecVersionNotFound,
                    None,
                    Some(project.get()),
                );
                Followed::VersionNotFound { project }
            }
            Ok(RefLookup::NotACommit) => {
                log(
                    "not_a_commit",
                    ErrorCode::SpecVersionNotFound,
                    None,
                    Some(project.get()),
                );
                Followed::VersionNotFound { project }
            }
            // Review X1: a git failure (busy, timeout, storage) on a referenced
            // repository is this entry's `unavailable`, never the request's
            // status (an existence oracle); the real failure is logged at warn
            // with its code.
            Err(e) => {
                tracing::warn!(
                    target: "forge_checks",
                    event = "forge.spec_refs.unresolved",
                    error_code = ErrorCode::SpecRepositoryUnavailable.id(),
                    git_error_code = e.code().id(),
                    git_error = %e,
                    request_id = %ctx.request_id,
                    project_id = ctx.project.get(),
                    referenced_project_id = project.get(),
                    referenced_repo_id = repository.id.get(),
                    reader = reader.name(),
                    line,
                    pin = pin.kind(),
                    reason = "git_failed",
                    "a .forge/repo.yml reference could not be read; shown as unavailable"
                );
                Followed::Unavailable
            }
        },
    )
}

/// forge-core's project access → forge-auth's loaded project.
fn loaded(id: ProjectId, access: ProjectAccess) -> LoadedProject {
    LoadedProject {
        id,
        visibility: access.visibility,
        role: access.role,
        pending_deletion: access.pending_deletion,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)] // tests may fail loudly (rust/testing.md)
mod tests {
    #[test]
    fn the_repo_config_path_is_a_tree_path() {
        let path = super::repo_config_path().expect("a tree path");
        assert_eq!(path.as_str(), forge_core::repo_config::REPO_CONFIG_PATH);
    }
}
