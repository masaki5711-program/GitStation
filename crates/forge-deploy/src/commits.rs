//! What may be deployed (M4 C11 review, ADR-0031 amendment 2026-11-13): a
//! commit of the project reachable from a protected branch — an applied
//! merge request merge or a commit pushed to a protected branch; both
//! passed the branch protection. Read with git through forge-git's
//! browse API (argv, the repository path from the id, the browse
//! timeout): that a branch or tag reaches the commit (`resolve_ref`), the
//! commit itself (`log -n 1`) and one `merge-base` per
//! protected branch tip. Behind [`CommitSource`] so the service is tested
//! without a repository.

use std::future::Future;
use std::pin::Pin;

use forge_core::{ObjectId, ProjectAccessLoader as _, ProjectId, RepositoryState};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{Browser, RefKind, RepoPath};

/// The commit as git reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitFacts {
    pub subject: String,
    pub author_name: String,
    pub author_email: String,
    pub committer_email: String,
    /// A protected branch whose tip is the commit or a descendant of it
    /// (`None`: no protected branch holds it).
    pub protected_branch: Option<String>,
}

/// What [`CommitSource::facts`] returns.
pub type FactsFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<CommitFacts>, ForgeError>> + Send + 'a>>;

/// Reads a commit of a project (`Ok(None)`: no such commit).
pub trait CommitSource: Send + Sync {
    fn facts<'a>(&'a self, project: ProjectId, sha: &'a ObjectId) -> FactsFuture<'a>;
}

/// The most protected branches compared (one `merge-base` each).
const MAX_BRANCHES: usize = 50;

/// The real source: the project's repository through the browse API.
#[derive(Clone)]
pub struct GitCommits {
    db: Db,
    browse: Browser,
}

impl GitCommits {
    pub fn new(db: Db, browse: Browser) -> Self {
        Self { db, browse }
    }

    async fn read(
        &self,
        project: ProjectId,
        sha: &ObjectId,
    ) -> Result<Option<CommitFacts>, ForgeError> {
        let Some(record) = self.db.load_project(project, None).await? else {
            return Ok(None);
        };
        let repo = record.repository;
        if repo.state != RepositoryState::Ready {
            return Err(ForgeError::new(ErrorCode::RepositoryNotReady)
                .with_context("repo_id", repo.id.get())
                .with_context("state", repo.state.as_str()));
        }
        // First: a commit a branch or tag of THIS repository reaches. A
        // missing id or one of another repository would otherwise reach
        // `git log` and fail as FG-GIT-0003 (a 500 and an alert) instead
        // of FG-DEPLOY-0011; an undecided check answers like a missing id.
        match self
            .browse
            .resolve_ref(repo.id, sha.as_str(), "deploy")
            .await
        {
            Ok(_) => {}
            Err(e) if e.code() == ErrorCode::RepositoryPathNotResolved => return Ok(None),
            Err(e) => return Err(e),
        }
        let page = match self
            .browse
            .commits(repo.id, sha.as_str(), &RepoPath::root(), 1, None, "deploy")
            .await
        {
            Ok(page) => page,
            Err(e) if e.code() == ErrorCode::RepositoryPathNotResolved => return Ok(None),
            Err(e) => return Err(e),
        };
        let Some(info) = page.commits.into_iter().find(|c| c.oid == sha.as_str()) else {
            return Ok(None);
        };
        let rules = forge_db::protected_refs::rules_for(&self.db, project).await?;
        let refs = self.browse.refs(repo.id).await?;
        let mut protected_branch = None;
        for r in refs
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::Branch && !rules.levels(&r.name).is_empty())
            .take(MAX_BRANCHES)
        {
            let Some(tip) = r.commit.as_deref() else {
                continue;
            };
            let on_branch = if tip == sha.as_str() {
                true
            } else {
                let tip = ObjectId::parse(tip).map_err(|_| {
                    ForgeError::new(ErrorCode::Internal).with_context("reason", "tip_unparsable")
                })?;
                self.browse.merge_base(repo.id, sha, &tip).await?.as_ref() == Some(sha)
            };
            if on_branch {
                protected_branch = Some(r.name.as_str().to_owned());
                break;
            }
        }
        Ok(Some(CommitFacts {
            subject: info.subject,
            author_name: info.author_name,
            author_email: info.author_email,
            committer_email: info.committer_email,
            protected_branch,
        }))
    }
}

impl CommitSource for GitCommits {
    fn facts<'a>(&'a self, project: ProjectId, sha: &'a ObjectId) -> FactsFuture<'a> {
        Box::pin(self.read(project, sha))
    }
}
