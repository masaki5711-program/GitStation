//! Org bindings (ADR-0038 §2, §5): `GET|POST
//! /api/v1/{projects|groups}/{id}/org-bindings` and `DELETE
//! …/org-bindings/{binding_id}`. The reads are the routes' (`MemberList`);
//! adding and removing are `OrgBindingManage`, decided again on the locked
//! group or project (forge-db `org_bindings::*_decided`).

use forge_auth::{Action, Authorized, GroupCtx, LoadedGroup, ProjectCtx};
use forge_core::{GroupId, ProjectId};
use forge_db::org_bindings::{
    self, BindingTarget, BoundMember, NewBinding, PickUnit, StoredBinding, TargetFacts,
};
use forge_errors::{ErrorCode, ForgeError};

use crate::{account, caller, change_context, loaded, require, Hosting, WriteRequest};

/// `FG-SYS-0001`: the transaction locked another kind of target than the
/// proof's (a programming error, never a pass).
fn target_mismatch() -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", "org_binding_target_mismatch")
}

fn project_decide(
    proof: &Authorized<ProjectCtx>,
) -> impl FnOnce(&TargetFacts) -> Result<(), ForgeError> + '_ {
    move |facts| match facts {
        TargetFacts::Project(f) => {
            let fresh = ProjectCtx::new(loaded(&f.project));
            proof.reconfirm(account(f.account), fresh).map(drop)
        }
        TargetFacts::Group(_) => Err(target_mismatch()),
    }
}

fn group_decide(
    proof: &Authorized<GroupCtx>,
) -> impl FnOnce(&TargetFacts) -> Result<(), ForgeError> + '_ {
    move |facts| match facts {
        TargetFacts::Group(f) => {
            let fresh = GroupCtx::new(LoadedGroup {
                id: f.group.group,
                role: f.group.role,
                sight: proof.ctx().sight(),
            });
            proof.reconfirm(account(f.account), fresh).map(drop)
        }
        TargetFacts::Project(_) => Err(target_mismatch()),
    }
}

impl Hosting {
    /// The bindings of a group or project (read pool).
    pub async fn org_bindings(
        &self,
        target: BindingTarget,
    ) -> Result<Vec<StoredBinding>, ForgeError> {
        org_bindings::list(&self.db, target).await
    }

    /// The people the bindings of a group or project give a role to now
    /// (`true`: more than listed).
    pub async fn org_binding_members(
        &self,
        target: BindingTarget,
    ) -> Result<(Vec<BoundMember>, bool), ForgeError> {
        org_bindings::members(&self.db, target).await
    }

    /// The master's units for the picker.
    pub async fn org_units(
        &self,
        search: Option<&str>,
        plant: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PickUnit>, ForgeError> {
        org_bindings::units(&self.db, search, plant, limit).await
    }

    /// `POST /api/v1/projects/{id}/org-bindings` (`OrgBindingManage`).
    pub async fn create_project_org_binding(
        &self,
        request: &WriteRequest,
        proof: &Authorized<ProjectCtx>,
        new: &NewBinding,
    ) -> Result<StoredBinding, ForgeError> {
        require(proof.action(), Action::OrgBindingManage)?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let project: ProjectId = *proof.id();
        let (stored, ()) = org_bindings::create_decided(
            &self.db,
            &ctx,
            BindingTarget::Project(project),
            user,
            new,
            project_decide(proof),
        )
        .await?;
        Ok(stored)
    }

    /// `POST /api/v1/groups/{id}/org-bindings` (`OrgBindingManage`).
    pub async fn create_group_org_binding(
        &self,
        request: &WriteRequest,
        proof: &Authorized<GroupCtx>,
        new: &NewBinding,
    ) -> Result<StoredBinding, ForgeError> {
        require(proof.action(), Action::OrgBindingManage)?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let group: GroupId = *proof.id();
        let (stored, ()) = org_bindings::create_decided(
            &self.db,
            &ctx,
            BindingTarget::Group(group),
            user,
            new,
            group_decide(proof),
        )
        .await?;
        Ok(stored)
    }

    /// `DELETE /api/v1/projects/{id}/org-bindings/{binding_id}`
    /// (`OrgBindingManage`).
    pub async fn delete_project_org_binding(
        &self,
        request: &WriteRequest,
        proof: &Authorized<ProjectCtx>,
        binding: i64,
    ) -> Result<StoredBinding, ForgeError> {
        require(proof.action(), Action::OrgBindingManage)?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let project: ProjectId = *proof.id();
        let (stored, ()) = org_bindings::delete_decided(
            &self.db,
            &ctx,
            BindingTarget::Project(project),
            binding,
            user,
            project_decide(proof),
        )
        .await?;
        Ok(stored)
    }

    /// `DELETE /api/v1/groups/{id}/org-bindings/{binding_id}`
    /// (`OrgBindingManage`).
    pub async fn delete_group_org_binding(
        &self,
        request: &WriteRequest,
        proof: &Authorized<GroupCtx>,
        binding: i64,
    ) -> Result<StoredBinding, ForgeError> {
        require(proof.action(), Action::OrgBindingManage)?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let group: GroupId = *proof.id();
        let (stored, ()) = org_bindings::delete_decided(
            &self.db,
            &ctx,
            BindingTarget::Group(group),
            binding,
            user,
            group_decide(proof),
        )
        .await?;
        Ok(stored)
    }
}
