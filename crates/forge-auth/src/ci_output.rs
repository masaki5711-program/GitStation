//! Deciding a CI output (M4 C5, ADR-0029 decision 5): a job's log
//! (`CiLogRead`) and a pipeline's diagnosis (`PipelineDiagnosisRead`).
//! [`authorize_ci_output`] calls [`authorize`] once with
//! [`Resource::CiOutput`], so the rule — an AI agent audience reads a CI
//! output only when the pipeline's own mark and the default branch's tip
//! are both `Open` — lives in the single decision point; this holds none.
//!
//! It takes the route's proof (the project-level decision of the same
//! action, made by the dispatcher) and the two marks, nothing else about
//! the caller: the principal and the project view are the proof's.
//! The answer, [`CiOutputAllowed`], is the only way forge-pipelines reads
//! a log body or a diagnosis.

use forge_core::ProjectId;
use forge_errors::{ErrorCode, ForgeError};

use crate::action::Action;
use crate::authorize::authorize;
use crate::model::{CopyMark, Resource};
use crate::prove::{Authorized, ProjectCtx};

/// What [`authorize_ci_output`] allowed: this action's output of this
/// project. Only built there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CiOutputAllowed {
    action: Action,
    project: ProjectId,
}

impl CiOutputAllowed {
    pub fn action(&self) -> Action {
        self.action
    }

    pub fn project(&self) -> ProjectId {
        self.project
    }
}

/// `authorize(<the proof's principal>, <the proof's action>, CiOutput {
/// <the proof's project>, pipeline_mark, tip_mark, declared_agent })`.
/// `declared_agent`: the request declared an AI agent audience
/// (`Forge-Context-Audience: agent`; a person's token is then decided as an
/// agent's, `audience=header`). `FG-SYS-0001` (a
/// programming error, never a pass) when the proof is not a `CiLogRead` /
/// `PipelineDiagnosisRead` proof (`reason=proof_action_mismatch`). An agent
/// audience's refusal is `FG-AUTH-0022` with `route`, `reason`, `mark_of`
/// and `audience`; `project_id` is added here.
pub fn authorize_ci_output(
    proof: &Authorized<ProjectCtx>,
    pipeline_mark: CopyMark,
    tip_mark: CopyMark,
    declared_agent: bool,
) -> Result<CiOutputAllowed, ForgeError> {
    let action = proof.action();
    if !matches!(
        action,
        Action::CiLogRead | Action::PipelineDiagnosisRead | Action::CiArtifactRead
    ) {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "proof_action_mismatch")
            .with_context(
                "route_action",
                "CiLogRead|PipelineDiagnosisRead|CiArtifactRead",
            )
            .with_context("proof_action", action.name()));
    }
    let project = *proof.id();
    authorize(
        proof.principal(),
        action,
        &Resource::CiOutput {
            project: proof.ctx().view(),
            pipeline_mark,
            tip_mark,
            declared_agent,
        },
    )
    .map_err(|e| e.with_context("project_id", project.get()))?;
    Ok(CiOutputAllowed { action, project })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly

    use super::*;
    use crate::model::{AccountKind, Credential, Principal, Role, Scope, Visibility};
    use crate::prove::{prove, LoadedProject};
    use forge_core::UserId;

    const MARKS: [CopyMark; 5] = [
        CopyMark::NotMeasured,
        CopyMark::Open,
        CopyMark::Confidential,
        CopyMark::Unborn,
        CopyMark::Undecided,
    ];

    fn proof(principal: Principal, action: Action) -> Authorized<ProjectCtx> {
        let ctx = ProjectCtx::new(LoadedProject {
            id: ProjectId::new(7),
            visibility: Visibility::Private,
            role: Some(Role::Developer),
            pending_deletion: false,
        });
        prove(&principal, action, ctx).expect("allowed")
    }

    fn token(kind: AccountKind) -> Principal {
        Principal::authenticated(
            UserId::new(1),
            false,
            true,
            Credential::Token(vec![Scope::ReadApi]),
            kind,
        )
    }

    fn context<'a>(err: &'a ForgeError, key: &str) -> Option<&'a str> {
        err.context()
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }

    /// ADR-0029 obligation 4: an undecided mark (the pipeline's or the
    /// tip's) refuses an agent audience before any CI output is read —
    /// the log and the diagnosis alike, every agent audience alike.
    #[test]
    fn an_undecided_mark_refuses_every_agent_audience_every_ci_output() {
        let agents = [
            token(AccountKind::Human).as_agent(true),
            token(AccountKind::ServiceAccount),
            token(AccountKind::Bot),
        ];
        for action in [Action::CiLogRead, Action::PipelineDiagnosisRead] {
            for agent in &agents {
                let p = proof(agent.clone(), action);
                for (pipeline, tip, mark_of) in [
                    (CopyMark::Undecided, CopyMark::Open, "pipeline"),
                    (CopyMark::Open, CopyMark::Undecided, "tip"),
                    (CopyMark::Undecided, CopyMark::Undecided, "pipeline"),
                ] {
                    let err = authorize_ci_output(&p, pipeline, tip, false).expect_err("closed");
                    assert_eq!(err.code(), ErrorCode::RepositoryCopyWithheld);
                    assert_eq!(context(&err, "mark_of"), Some(mark_of));
                    assert_eq!(context(&err, "project_id"), Some("7"));
                }
            }
        }
    }

    #[test]
    fn an_agent_reads_only_when_both_marks_are_open() {
        let agent = token(AccountKind::Human).as_agent(true);
        for action in [Action::CiLogRead, Action::PipelineDiagnosisRead] {
            let p = proof(agent.clone(), action);
            for pipeline in MARKS {
                for tip in MARKS {
                    let got = authorize_ci_output(&p, pipeline, tip, false);
                    let open = pipeline == CopyMark::Open && tip == CopyMark::Open;
                    assert_eq!(got.is_ok(), open, "{action:?} {pipeline:?} {tip:?}");
                    if let Ok(allowed) = got {
                        assert_eq!(allowed.action(), action);
                        assert_eq!(allowed.project(), ProjectId::new(7));
                    }
                }
            }
        }
        let err = authorize_ci_output(
            &proof(agent.clone(), Action::CiLogRead),
            CopyMark::Confidential,
            CopyMark::Open,
            false,
        )
        .expect_err("pipeline");
        assert_eq!(context(&err, "route"), Some("ci_log"));
        assert_eq!(context(&err, "reason"), Some("pipeline_confidential"));
        assert_eq!(context(&err, "audience"), Some("agent_token"));
        let err = authorize_ci_output(
            &proof(agent, Action::PipelineDiagnosisRead),
            CopyMark::Open,
            CopyMark::Confidential,
            false,
        )
        .expect_err("tip");
        assert_eq!(context(&err, "route"), Some("ci_diagnosis"));
        assert_eq!(context(&err, "reason"), Some("tip_confidential"));
    }

    #[test]
    fn a_person_is_never_refused_by_the_marks() {
        let person = token(AccountKind::Human);
        for action in [Action::CiLogRead, Action::PipelineDiagnosisRead] {
            let p = proof(person.clone(), action);
            for pipeline in MARKS {
                for tip in MARKS {
                    assert!(authorize_ci_output(&p, pipeline, tip, false).is_ok());
                }
            }
        }
    }

    /// `gstn mcp` declares an agent audience on every request: a person's
    /// own token is then decided as an agent's (fail closed), and the
    /// refusal says why (`audience=header`).
    #[test]
    fn a_declared_agent_audience_is_decided_as_an_agent() {
        let person = token(AccountKind::Human);
        for action in [Action::CiLogRead, Action::PipelineDiagnosisRead] {
            let p = proof(person.clone(), action);
            assert!(authorize_ci_output(&p, CopyMark::Open, CopyMark::Open, true).is_ok());
            for (pipeline, tip) in [
                (CopyMark::Confidential, CopyMark::Open),
                (CopyMark::Open, CopyMark::NotMeasured),
                (CopyMark::Undecided, CopyMark::Open),
            ] {
                let err = authorize_ci_output(&p, pipeline, tip, true).expect_err("declared");
                assert_eq!(err.code(), ErrorCode::RepositoryCopyWithheld);
                assert_eq!(context(&err, "audience"), Some("header"));
            }
        }
    }

    #[test]
    fn only_a_ci_output_proof_decides() {
        let p = proof(token(AccountKind::Human), Action::PipelineRead);
        let err =
            authorize_ci_output(&p, CopyMark::Open, CopyMark::Open, false).expect_err("proof");
        assert_eq!(err.code(), ErrorCode::Internal);
        assert_eq!(context(&err, "reason"), Some("proof_action_mismatch"));
    }
}
