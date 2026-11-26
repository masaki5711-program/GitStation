//! Reading a stored incident diagnosis (plan 19 §3.2 / §8.2, M5 L5-7b):
//! `GET /api/v1/admin/diagnoses/{advice_id}` and `gstn ops diagnosis <id>`.
//!
//! The route proves `OpsDiagnosisRead` on the instance (an administrator; a
//! token needs the `admin` scope; anyone else 404). The advice is then read
//! on its **own** subject (`forge_db::llm::advice_decided`): only an
//! advice whose subject is an incident and whose purpose is the incident
//! diagnosis is served here — any other id is "not found", so this route
//! never opens a merge request's or a deployment's advice. The decision on
//! that subject goes through `forge_auth::authorize_advice` once more, and
//! its reader kind withholds a confidential diagnosis's words from an agent
//! (plan 19 §11 question 6: an admin agent token reads `internal` ones only).
//! Reading changes nothing.

use forge_auth::{authorize_advice, Action, AdviceReader, Authorized, InstanceCtx, ResourceCtx};
use forge_core::llm::{InputClass, Purpose, SubjectKind};
use forge_db::llm::{AdviceView, ReaderKind, StoredAdvice};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

/// What a reader gets.
#[derive(Debug, Clone, PartialEq)]
pub enum Diagnosis {
    Shown(Box<StoredAdvice>),
    /// A confidential diagnosis asked for by an agent: that it exists only.
    Withheld {
        id: i64,
        input_class: InputClass,
    },
}

/// The diagnosis reads (one per server).
#[derive(Debug, Clone)]
pub struct Diagnoses {
    db: Db,
}

fn not_found(id: i64) -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", "diagnosis")
        .with_context("advice_id", id)
}

impl Diagnoses {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// The diagnosis `id` (module docs). `declared_agent`:
    /// `Forge-Context-Audience: agent`. Not an incident diagnosis, or no
    /// such advice: `FG-REPO-0005`-style "not found" (`NotFound`).
    pub async fn read(
        &self,
        proof: &Authorized<InstanceCtx>,
        id: i64,
        declared_agent: bool,
    ) -> Result<Diagnosis, ForgeError> {
        if proof.action() != Action::OpsDiagnosisRead {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "proof_action_mismatch")
                .with_context("action", proof.action().name()));
        }
        let resource = proof.ctx().resource();
        let mut reader = None;
        // The store is asked for the words in any case (`Person`): the
        // purpose is checked first, then a confidential one is withheld
        // from an agent here — so an id of another purpose is "not found"
        // to everyone, never "withheld".
        let view = forge_db::llm::advice_decided(&self.db, id, |subject| {
            if subject.kind != SubjectKind::Incident || subject.id.is_some() {
                return Err(not_found(id));
            }
            reader = Some(authorize_advice(
                proof.principal(),
                Action::OpsDiagnosisRead,
                &resource,
                declared_agent,
            )?);
            Ok(ReaderKind::Person)
        })
        .await?;
        let a = match view {
            Some(AdviceView::Shown(a)) if a.purpose == Purpose::IncidentDiagnosis => a,
            Some(AdviceView::Shown(_)) | Some(AdviceView::Withheld { .. }) | None => {
                return Err(not_found(id))
            }
        };
        match reader {
            Some(AdviceReader::Person) => Ok(Diagnosis::Shown(a)),
            Some(AdviceReader::Agent) if a.input_class.shown_to_agents() => Ok(Diagnosis::Shown(a)),
            Some(AdviceReader::Agent) => Ok(Diagnosis::Withheld {
                id: a.id,
                input_class: a.input_class,
            }),
            // The store decided without calling `decide`: cannot happen;
            // never shown on an undecided read.
            None => Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "advice_read_undecided")
                .with_context("advice_id", id)),
        }
    }
}

#[cfg(test)]
#[path = "diagnoses_tests.rs"]
mod tests;
