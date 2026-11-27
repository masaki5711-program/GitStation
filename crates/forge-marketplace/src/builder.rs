//! The agent builder (plan 16 A7, ADR-0032 Amendment A7): a project's
//! Claude Code configuration built from published, signed part versions.
//!
//! - [`Marketplace::plan_build`] (the Dry-Run and the first half of the
//!   build): reads the selected versions and their files from the
//!   database (a revoked one is `FG-AGENT-0006`, one that is not there
//!   `FG-AGENT-0008 part_not_found`), verifies and builds them with
//!   forge-agent's [`compile`] (digest, signature, selection, merge — the
//!   first of the three verifications), reads the managed area at the
//!   default branch's tip (`Browser::agent_config`) and lists what the
//!   build adds, changes and removes. `custom` files of the tip's lock
//!   that are still there unchanged are kept. Nothing is written.
//! - The route then writes the plan as one bot commit on a new branch and
//!   opens the merge request for the person who asked (forge-web), and
//!   [`Marketplace::record_build`] keeps the run in `forge.agent_builds`.
//! - [`Marketplace::recommend_for`]: forge-agent's rules-only
//!   [`recommend`] on the stable catalog, from the tip's lock.

use std::collections::BTreeSet;

use forge_agent::{
    compile, recommend, CatalogVersion, CompileInput, Compiled, CustomFile, KeyRing, Lock,
    Recommendation, SourcePart, DEFAULT_ADAPTER, LOCK_PATH,
};
use forge_core::{ObjectId, ProjectId, RefName, RepoId, UserId};
use forge_db::agent_builder::{self as builds, BuildOutcome, NewBuild, StoredBuild};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{AgentConfigListing, RefLookup};

use crate::{bounded, internal, problems_error, Marketplace};

/// The most files the managed area of a tip is read with.
pub const MAX_TIP_FILES: usize = 2000;
/// The most bytes of the managed area of a tip.
pub const MAX_TIP_BYTES: u64 = 8 * 1024 * 1024;

/// The bot the builder's commits are authored and committed by (the push
/// is recorded for the person who asked).
pub const BUILDER_NAME: &str = "forge-agent-builder";
pub const BUILDER_EMAIL: &str = "agent-builder@forge.invalid";

/// One selected part version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selected {
    pub slug: String,
    pub version: String,
    /// `user` (picked on the screen) or `rule` (the recommendation).
    pub selected_by: String,
    pub reason: Option<String>,
}

/// One file the build changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    /// `added`, `modified` or `removed`.
    pub change: &'static str,
}

/// One part of the plan, as the Dry-Run shows it (the capability marks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedPart {
    pub slug: String,
    pub kind: String,
    pub version: String,
    pub channel: String,
    pub capabilities: Vec<String>,
}

/// What a build would write (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildPlan {
    /// The branch the merge request targets (`HEAD`'s).
    pub target: RefName,
    /// Its tip the plan was made against.
    pub base: ObjectId,
    pub compiled: Compiled,
    /// What changes, by path.
    pub changes: Vec<FileChange>,
    /// The files to write (path, bytes) and the paths to remove.
    pub writes: Vec<(String, Vec<u8>)>,
    pub removes: Vec<String>,
    pub parts: Vec<PlannedPart>,
    pub adapter_version_id: Option<i64>,
    /// The selection as recorded (`forge.agent_builds.selection`).
    pub selection: serde_json::Value,
}

fn unresolved(reason: &'static str, subject: &str) -> ForgeError {
    ForgeError::new(ErrorCode::SelectionUnresolved)
        .with_context("reason", reason)
        .with_context("subject", subject.chars().take(200).collect::<String>())
}

/// A selection's own shape (before any read).
fn check_selection(selection: &[Selected]) -> Result<(), ForgeError> {
    if selection.is_empty() {
        return Err(unresolved("adapter_missing", ""));
    }
    if selection.len() > forge_agent::MAX_SELECTED_PARTS {
        return Err(unresolved("too_many_parts", &selection.len().to_string()));
    }
    for s in selection {
        let fine = |t: &str, max: usize| {
            !t.is_empty()
                && t.len() <= max
                && t.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
        };
        if !fine(&s.slug, 64) || !fine(&s.version, 64) {
            return Err(unresolved(
                "part_not_found",
                &format!("{}@{}", s.slug, s.version),
            ));
        }
        if !matches!(s.selected_by.as_str(), "user" | "rule") {
            return Err(ForgeError::new(ErrorCode::SelectionUnresolved)
                .with_context("reason", "selected_by_invalid"));
        }
        if s.reason
            .as_deref()
            .is_some_and(|r| r.chars().count() > 200 || r.chars().any(char::is_control))
        {
            return Err(ForgeError::new(ErrorCode::SelectionUnresolved)
                .with_context("reason", "reason_invalid"));
        }
    }
    Ok(())
}

impl Marketplace {
    /// The default branch and its tip (`FG-REPO-0023 reason=ref_not_found`
    /// for an empty repository: nothing to build on).
    async fn default_tip(&self, repo: RepoId) -> Result<(RefName, ObjectId), ForgeError> {
        let list = self.browse.refs(repo).await?;
        let not_found = || {
            ForgeError::new(ErrorCode::RepositoryPathNotResolved)
                .with_context("reason", "ref_not_found")
                .with_context("param", "default_branch")
        };
        let branch = list.default_branch.as_deref().ok_or_else(not_found)?;
        let name = RefName::parse(&format!("refs/heads/{branch}")).map_err(|_| not_found())?;
        // A reader's refresh (the cached list may be older than the tip).
        self.browse.forget_refs(repo);
        match self.browse.find_ref(repo, &name).await? {
            RefLookup::Commit(rev) => {
                let tip =
                    ObjectId::parse(&rev.commit).map_err(|_| internal("invalid_commit_id"))?;
                Ok((name, tip))
            }
            RefLookup::Missing | RefLookup::NotACommit => Err(not_found()),
        }
    }

    /// The managed area of `commit`; a part of it that cannot be read
    /// whole is `FG-AGENT-0007 tree_unreadable` (nothing is built on what
    /// was not read).
    async fn managed_at(
        &self,
        repo: RepoId,
        commit: &ObjectId,
    ) -> Result<AgentConfigListing, ForgeError> {
        let found = self
            .browse
            .agent_config(repo, commit.as_str(), MAX_TIP_FILES, MAX_TIP_BYTES)
            .await?;
        if found.bounded() {
            let what = found
                .unreadable
                .first()
                .map(|(r, p)| format!("{r} {p}"))
                .unwrap_or_default();
            return Err(ForgeError::new(ErrorCode::AgentLockMismatch)
                .with_context("reason", "tree_unreadable")
                .with_context("subject", what));
        }
        Ok(found)
    }

    /// The Dry-Run of `selection` for `project`'s repository `repo`
    /// (module docs). Errors: the selection (`FG-AGENT-0008`, `-0006`),
    /// a part that does not verify (`FG-AGENT-0002` / `-0003`), the tip
    /// (`FG-REPO-0023`, `FG-AGENT-0007 tree_unreadable`), git and the
    /// database; all within one bound (`FG-WEB-0002 operation=agent_build`).
    pub async fn plan_build(
        &self,
        project: ProjectId,
        repo: RepoId,
        selection: &[Selected],
    ) -> Result<BuildPlan, ForgeError> {
        bounded("agent_build", self.plan_inner(project, repo, selection)).await
    }

    async fn plan_inner(
        &self,
        project: ProjectId,
        repo: RepoId,
        selection: &[Selected],
    ) -> Result<BuildPlan, ForgeError> {
        check_selection(selection)?;
        let wanted: Vec<(String, String)> = selection
            .iter()
            .map(|s| (s.slug.clone(), s.version.clone())) // clone: the query owns them
            .collect();
        let stored = builds::versions(&self.db, &wanted).await?;
        let mut versions = Vec::with_capacity(selection.len());
        for s in selection {
            let Some(v) = stored
                .iter()
                .find(|v| v.slug == s.slug && v.version == s.version)
            else {
                return Err(unresolved(
                    "part_not_found",
                    &format!("{}@{}", s.slug, s.version),
                ));
            };
            if v.state != "published" {
                return Err(ForgeError::new(ErrorCode::PartVersionRevoked)
                    .with_context("reason", "revoked")
                    .with_context("slug", v.slug.clone()) // clone: the error owns it
                    .with_context("version", v.version.clone())); // clone: as above
            }
            versions.push((s, v));
        }
        let ids: Vec<i64> = versions.iter().map(|(_, v)| v.id).collect();
        let contents = builds::contents(&self.db, &ids).await?;
        let mut parts = Vec::with_capacity(versions.len());
        for (s, v) in &versions {
            let requires = v
                .manifest
                .get("requires")
                .and_then(serde_json::Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|r| r.as_str().map(str::to_owned))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            parts.push(SourcePart {
                slug: v.slug.clone(),       // clone: the build owns its parts
                kind: v.kind.clone(),       // clone: as above
                version: v.version.clone(), // clone: as above
                requires,
                capabilities: v.capabilities.clone(), // clone: as above
                digest: v.digest.clone(),             // clone: as above
                key_id: v.key_id.clone(),             // clone: as above
                signature: v.signature.clone(),       // clone: as above
                files: contents
                    .iter()
                    .filter(|(id, _, _)| *id == v.id)
                    .map(|(_, p, c)| (p.clone(), c.clone())) // clone: as above
                    .collect(),
                selected_by: s.selected_by.clone(), // clone: as above
                reason: s.reason.clone(),           // clone: as above
            });
        }
        let ring = server_ring(&self.db).await?;

        let (target, base) = self.default_tip(repo).await?;
        let tip = self.managed_at(repo, &base).await?;
        // The tip lock's `custom` files that are still there, unchanged.
        let custom: Vec<CustomFile> = tip
            .lock
            .as_deref()
            .and_then(|b| Lock::parse(b).ok())
            .map(|lock| {
                lock.custom
                    .into_iter()
                    .filter(|c| {
                        tip.files
                            .get(&c.path)
                            .is_some_and(|b| hex(&forge_agent::sha256(b)) == c.sha256)
                    })
                    .map(|c| CustomFile {
                        path: c.path,
                        sha256: c.sha256,
                        approved_by: c.approved_by,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let compiled = compile(&CompileInput {
            project_id: project.get(),
            parts: &parts,
            custom: &custom,
            keys: &ring,
        })
        .map_err(|p| problems_error(&p))?;

        // What changes: the build's files and lock against the tip's.
        let kept: BTreeSet<&str> = custom.iter().map(|c| c.path.as_str()).collect();
        let written = compiled.written();
        let mut changes = Vec::new();
        let mut writes = Vec::new();
        for (path, bytes) in &written {
            let now = if path == LOCK_PATH {
                tip.lock.as_ref()
            } else {
                tip.files.get(path)
            };
            match now {
                Some(b) if b == bytes => {}
                Some(_) => {
                    changes.push(FileChange {
                        path: path.clone(), // clone: the plan owns it
                        change: "modified",
                    });
                    writes.push((path.clone(), bytes.clone())); // clone: as above
                }
                None => {
                    changes.push(FileChange {
                        path: path.clone(), // clone: as above
                        change: "added",
                    });
                    writes.push((path.clone(), bytes.clone())); // clone: as above
                }
            }
        }
        let mut removes = Vec::new();
        let unreadable = tip
            .unreadable
            .iter()
            .filter(|(r, _)| *r == "not_a_regular_file")
            .map(|(_, p)| p);
        for path in tip.files.keys().chain(unreadable) {
            if written.contains_key(path) || kept.contains(path.as_str()) {
                continue;
            }
            changes.push(FileChange {
                path: path.clone(), // clone: as above
                change: "removed",
            });
            removes.push(path.clone()); // clone: as above
        }
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        if writes.len() + removes.len() > forge_git::file_commit::MAX_TREE_CHANGE_FILES {
            return Err(unresolved("too_many_files", &changes.len().to_string()));
        }
        let adapter_version_id = versions
            .iter()
            .find(|(_, v)| v.kind == "adapter")
            .map(|(_, v)| v.id);
        let planned = versions
            .iter()
            .map(|(_, v)| PlannedPart {
                slug: v.slug.clone(),                 // clone: the plan owns it
                kind: v.kind.clone(),                 // clone: as above
                version: v.version.clone(),           // clone: as above
                channel: v.channel.clone(),           // clone: as above
                capabilities: v.capabilities.clone(), // clone: as above
            })
            .collect();
        let selection_json = serde_json::Value::Array(
            selection
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "slug": s.slug, "version": s.version,
                        "selected_by": s.selected_by, "reason": s.reason,
                    })
                })
                .collect(),
        );
        Ok(BuildPlan {
            target,
            base,
            compiled,
            changes,
            writes,
            removes,
            parts: planned,
            adapter_version_id,
            selection: selection_json,
        })
    }

    /// The rules-only recommendation for `repo` (module docs): from the
    /// lock at the default branch's tip (its adapter and parts), or the
    /// default base model.
    pub async fn recommend_for(&self, repo: RepoId) -> Result<Recommendation, ForgeError> {
        bounded("agent_recommend", async {
            let current: Option<Lock> = match self.default_tip(repo).await {
                Ok((_, tip)) => self
                    .managed_at(repo, &tip)
                    .await?
                    .lock
                    .as_deref()
                    .and_then(|b| Lock::parse(b).ok()),
                // An empty repository: nothing locked yet.
                Err(e) if e.code() == ErrorCode::RepositoryPathNotResolved => None,
                Err(e) => return Err(e),
            };
            let adapter = current
                .as_ref()
                .and_then(|l| l.parts.iter().find(|p| p.kind == "adapter"))
                .map_or_else(|| DEFAULT_ADAPTER.to_owned(), |p| p.slug.clone()); // clone: owned
            let slugs: Vec<String> = current
                .map(|l| l.parts.into_iter().map(|p| p.slug).collect())
                .unwrap_or_default();
            let catalog: Vec<CatalogVersion> = builds::stable_versions(&self.db)
                .await?
                .into_iter()
                .map(|v| CatalogVersion {
                    requires: v
                        .manifest
                        .get("requires")
                        .and_then(serde_json::Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|r| r.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default(),
                    slug: v.slug,
                    kind: v.kind,
                    version: v.version,
                })
                .collect();
            recommend(&catalog, &adapter, &slugs).map_err(|p| problems_error(&p))
        })
        .await
    }

    /// Keep one builder run (`forge.agent_builds`).
    #[allow(clippy::too_many_arguments)] // one record's fields
    pub async fn record_build(
        &self,
        project: ProjectId,
        requested_by: UserId,
        plan: Option<&BuildPlan>,
        selection: &serde_json::Value,
        dry_run: bool,
        outcome: &BuildOutcome<'_>,
        request_id: &str,
    ) -> Result<i64, ForgeError> {
        builds::record_build(
            &self.db,
            &NewBuild {
                project,
                requested_by,
                adapter_version_id: plan.and_then(|p| p.adapter_version_id),
                selection,
                dry_run,
                request_id,
            },
            outcome,
        )
        .await
    }

    /// The project's newest builds (at most `limit`).
    pub async fn builds(
        &self,
        project: ProjectId,
        limit: i64,
    ) -> Result<Vec<StoredBuild>, ForgeError> {
        builds::builds(&self.db, project, limit).await
    }
}

/// The registered signing keys as forge-agent's ring (revoked keys
/// marked): the server's own verification of what it builds.
async fn server_ring(db: &forge_db::Db) -> Result<KeyRing, ForgeError> {
    let keys = forge_db::agent_marketplace::signing_keys(db).await?;
    let listed: Vec<serde_json::Value> = keys
        .iter()
        .map(|k| {
            let mut v = serde_json::json!({"kid": k.kid, "public_key": k.public_key});
            if k.revoked {
                v["revoked_at"] = serde_json::Value::from("revoked");
            }
            v
        })
        .collect();
    KeyRing::parse(serde_json::json!({ "keys": listed }).to_string().as_bytes())
        .map_err(|p| problems_error(&[p]))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
