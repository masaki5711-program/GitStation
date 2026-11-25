//! A project's wiki (plan 20 W2, ADR-0040 §5, option W-C): an ordinary
//! project in the owner project's namespace, linked by
//! `forge.project_wikis`. The route (`POST /api/v1/projects/{id}/wiki`)
//! runs the steps in order and links **last**, so a failure leaves no link:
//!
//! 1. [`Hosting::create_wiki_project`] — the registry proved
//!    `ProjectWikiLink` on the owner; the wiki project is then created as
//!    `ProjectCreate` on the owner's namespace, proved here with the
//!    caller's own principal (`forge_auth::prove`) and decided again inside
//!    the create transaction (`create_project`). A taken path is
//!    `FG-KNOW-0003 reason=name_taken`.
//! 2. [`Hosting::wiki_provision_target`] / [`Hosting::mark_wiki_ready`] —
//!    the repository is created now (the queued `repository.provision` job
//!    finds it ready and does nothing; the storage step adopts a concurrent
//!    attempt with the same token).
//! 3. the first commit (the route: git).
//! 4. [`Hosting::protect_wiki_main`] — `refs/heads/main`, Developer may push
//!    (Q3: direct commits), nobody force-pushes or deletes it; proved as
//!    `ProtectedRefManage` on the new project with the caller's principal
//!    (its creator, Owner) and decided again in its transaction.
//! 5. [`Hosting::link_wiki`] — decided again on the locked owner project.
//!
//! [`Hosting::unlink_wiki`] removes the link only.

use forge_auth::{
    prove, Action, Authorized, LoadedGroup, LoadedParent, LoadedProject, LoadedProtection,
    LoadedUserNamespace, ParentCtx, ProjectCtx, RefProtectionCtx,
};
use forge_core::{ProjectId, ProtectedRefId, RefPattern, RepoId, Role};
use forge_db::change::{Actor, ChangeContext};
use forge_db::repositories;
pub use forge_db::repositories::{MarkReady, ProvisionTarget};
use forge_db::resolve::{self, ParentRecord};
use forge_db::wikis::{self, WikiLink};
use forge_errors::{ErrorCode, ForgeError};

use crate::{
    account, caller, change_context, loaded, require, CreatedProject, Hosting, NewProjectFields,
    WriteRequest,
};

/// The branch a wiki is created with and protected on.
pub const WIKI_BRANCH: &str = "refs/heads/main";

/// What the caller asked for (both optional).
#[derive(Debug, Clone, Default)]
pub struct NewWikiFields {
    /// The wiki project's path segment; default `<owner segment>-wiki`.
    pub path: Option<String>,
    /// Default `<owner display name> Wiki`.
    pub display_name: Option<String>,
}

/// The wiki project as created (step 1).
#[derive(Debug, Clone)]
pub struct CreatedWiki {
    pub project: CreatedProject,
    pub display_name: String,
    /// The owner's namespace path (the first page's `owner`).
    pub owner_namespace: String,
}

fn internal(reason: &'static str, owner: ProjectId) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("project_id", owner.get())
}

impl Hosting {
    /// The link `project` is part of, as owner or as wiki.
    pub async fn wiki_link(&self, project: ProjectId) -> Result<Option<WikiLink>, ForgeError> {
        wikis::link_of(&self.db, project).await
    }

    /// Step 1 (module docs): create the wiki project for the owner of
    /// `proof` (`ProjectWikiLink`). Refused early (`FG-KNOW-0003`) when the
    /// owner already has a wiki or is a wiki; the link itself is written
    /// only by [`Hosting::link_wiki`].
    pub async fn create_wiki_project(
        &self,
        request: &WriteRequest,
        proof: &Authorized<ProjectCtx>,
        fields: &NewWikiFields,
    ) -> Result<CreatedWiki, ForgeError> {
        require(proof.action(), Action::ProjectWikiLink)?;
        let user = caller(proof)?;
        let owner = *proof.id();
        if let Some(link) = wikis::link_of(&self.db, owner).await? {
            let reason = if link.project == owner {
                "already_linked"
            } else {
                "chain"
            };
            return Err(
                wikis::refused(reason, owner).with_context("wiki_project_id", link.wiki.get())
            );
        }
        let record = self
            .project(owner, Some(user))
            .await?
            .ok_or_else(|| internal("owner_project_missing", owner))?;
        let (parent_ns, segment) = wikis::owner_placement(&self.db, owner)
            .await?
            .ok_or_else(|| internal("owner_namespace_missing", owner))?;
        let path = match &fields.path {
            Some(path) => path.clone(), // clone: forge-db owns its request
            None => forge_core::wiki::default_wiki_segment(&segment)
                .ok_or_else(|| wikis::refused("path_required", owner))?,
        };
        let display_name = fields
            .display_name
            .clone() // clone: as above
            .unwrap_or_else(|| format!("{} Wiki", record.display_name));
        let parent = match resolve::load_parent(&self.db, parent_ns, Some(user)).await? {
            Some(ParentRecord::Group {
                namespace,
                group,
                role,
                sight,
            }) => LoadedParent::Group {
                namespace,
                group: LoadedGroup {
                    id: group,
                    role,
                    sight,
                },
            },
            Some(ParentRecord::User {
                namespace,
                owner: user_owner,
                owner_kind,
            }) => LoadedParent::User(LoadedUserNamespace {
                namespace,
                owner: user_owner,
                owner_kind,
            }),
            Some(ParentRecord::Project) | None => {
                return Err(internal("owner_parent_not_a_namespace", owner))
            }
        };
        // The caller's own decision to create in that namespace.
        let create = prove(
            proof.principal(),
            Action::ProjectCreate,
            ParentCtx::new(parent),
        )?;
        let new = NewProjectFields {
            path,
            display_name: display_name.clone(), // clone: the answer keeps it
            description: format!("Wiki of {}", record.path.full_path()),
            visibility: record.access.visibility,
        };
        let created = self
            .create_project(request, &create, &new)
            .await
            .map_err(|e| {
                if e.code() == ErrorCode::NamespacePathTaken {
                    wikis::refused("name_taken", owner).with_source(e)
                } else {
                    e
                }
            })?;
        let full = record.path.full_path();
        let owner_namespace = full
            .rsplit_once('/')
            .map_or_else(|| full.clone(), |(ns, _)| ns.to_owned()); // clone: no parent segment
        Ok(CreatedWiki {
            project: created,
            display_name,
            owner_namespace,
        })
    }

    /// Step 2: the repository's provisioning facts (state and token).
    pub async fn wiki_provision_target(&self, repo: RepoId) -> Result<ProvisionTarget, ForgeError> {
        repositories::provision_target(&self.db, repo).await
    }

    /// Step 2: mark the repository ready (audited, `repository.created`; a
    /// no-op when the job was faster), bounded by `timeout`.
    pub async fn mark_wiki_ready(
        &self,
        request: &WriteRequest,
        repo: RepoId,
        timeout: std::time::Duration,
    ) -> Result<MarkReady, ForgeError> {
        let mut ctx = ChangeContext::new(Actor::System, request.request_id.clone());
        ctx.actor_label = Some("forge-web.wiki".to_owned());
        repositories::mark_ready_within(&self.db, &ctx, repo, timeout).await
    }

    /// Step 4: protect the wiki's `main` (Developer may push; no force
    /// push, no deletion), proved as `ProtectedRefManage` on the new
    /// project with the caller's principal (`proof`: `ProjectWikiLink` on
    /// the owner, whose principal created the wiki).
    pub async fn protect_wiki_main(
        &self,
        request: &WriteRequest,
        proof: &Authorized<ProjectCtx>,
        wiki: ProjectId,
    ) -> Result<ProtectedRefId, ForgeError> {
        require(proof.action(), Action::ProjectWikiLink)?;
        let user = caller(proof)?;
        let record = self
            .project(wiki, Some(user))
            .await?
            .ok_or_else(|| internal("wiki_project_missing", *proof.id()))?;
        let project = LoadedProject {
            id: wiki,
            visibility: record.access.visibility,
            role: record.access.role,
            pending_deletion: record.access.pending_deletion,
        };
        let ctx = RefProtectionCtx::new(
            project,
            LoadedProtection {
                rule: None,
                from: None,
                to: Some(Role::Developer),
            },
        );
        let manage = prove(proof.principal(), Action::ProtectedRefManage, ctx)?;
        let pattern = RefPattern::parse(WIKI_BRANCH)?;
        self.create_protected_ref(request, &manage, &pattern).await
    }

    /// Step 5: link `wiki` to the owner of `proof`, decided again on the
    /// locked owner project (`FG-KNOW-0003` refusals write nothing).
    pub async fn link_wiki(
        &self,
        request: &WriteRequest,
        proof: &Authorized<ProjectCtx>,
        wiki: ProjectId,
    ) -> Result<(), ForgeError> {
        require(proof.action(), Action::ProjectWikiLink)?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        wikis::link_decided(&self.db, &ctx, *proof.id(), wiki, user, |facts| {
            let fresh = ProjectCtx::new(loaded(&facts.project));
            proof.reconfirm(account(facts.account), fresh).map(drop)
        })
        .await
    }

    /// `DELETE /api/v1/projects/{id}/wiki`: unlink the owner's wiki; the
    /// wiki project stays. Returns the unlinked wiki project.
    pub async fn unlink_wiki(
        &self,
        request: &WriteRequest,
        proof: &Authorized<ProjectCtx>,
    ) -> Result<ProjectId, ForgeError> {
        require(proof.action(), Action::ProjectWikiLink)?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let (wiki, ()) = wikis::unlink_decided(&self.db, &ctx, *proof.id(), user, |facts| {
            let fresh = ProjectCtx::new(loaded(&facts.project));
            proof.reconfirm(account(facts.account), fresh).map(drop)
        })
        .await?;
        Ok(wiki)
    }
}
