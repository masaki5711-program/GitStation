//! `GET /api/v1/admin/diagnoses/{advice_id}` (plan 19 §3.2 / §8.2, M5
//! L5-7b): an incident first diagnosis `forge diagnose` stored, for
//! `gstn ops diagnosis <id>` and the incident record.
//!
//! `OpsDiagnosisRead` on the instance (an administrator; a token needs the
//! `admin` scope; anyone else 404 — the matrix rows). The advice is read on
//! its own subject by `forge_llm::diagnoses` (only incident diagnoses; any
//! other id 404) and a confidential one's words are withheld from an agent
//! (an agent's token, or `Forge-Context-Audience: agent`). Read-only.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::InstanceCtx;
use forge_core::parse_positive_id;
use forge_errors::{ErrorCode, ForgeError};
use forge_llm::diagnoses::{Diagnoses, Diagnosis};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::Dispatched;
use crate::routes::write::{bounded, json, no_store};

/// `GET` one stored incident diagnosis.
pub const ONE: &str = "/api/v1/admin/diagnoses/{advice_id}";
const PREFIX: &str = "/api/v1/admin/diagnoses/";
/// The label every AI advice carries (plan 19 §4).
pub const ADVICE_LABEL: &str = "AI の助言（提案）";
/// What a withheld diagnosis says instead of its words.
pub const WITHHELD_NOTE: &str = "機密の入力のため表示しません";

/// One cause candidate, as checked and stored.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = DiagnosisCandidateBody)]
pub struct CandidateBody {
    /// `critical`, `high`, `medium` or `low` (the impact).
    pub severity: String,
    /// The fact part it cites (`log`, `catalog:<code>`, `runbook:<name>`,
    /// `incident:<id>`, `changes`, `doctor`).
    #[schema(required = true)]
    pub path: Option<String>,
    /// The cited log line (`L<n>`), for `log`.
    #[schema(required = true)]
    pub line: Option<u32>,
    /// The evidence, as found in the facts.
    pub quote: String,
    /// The cause and the read-only command to look at next.
    pub message: String,
    /// How likely (0 to 1).
    pub confidence: f64,
}

/// A stored incident diagnosis.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = IncidentDiagnosisBody)]
pub struct DiagnosisBody {
    pub id: i64,
    /// `shown`, or `withheld` (a confidential diagnosis read by an agent).
    pub state: String,
    /// Always "AI の助言（提案）": advice, never a decision.
    pub label: String,
    /// `public`, `internal` or `confidential`.
    pub input_class: String,
    /// Why the words are not shown (withheld only).
    #[schema(required = true)]
    pub note: Option<String>,
    /// The model (`name:tag`); null when withheld.
    #[schema(required = true)]
    pub model: Option<String>,
    #[schema(required = true)]
    pub prompt_version: Option<i32>,
    /// UTC, `YYYY-MM-DDTHH:MM:SSZ`; null when withheld.
    #[schema(required = true)]
    pub created_at: Option<String>,
    #[schema(required = true)]
    pub summary: Option<String>,
    /// At most three, most likely first (empty when withheld).
    pub candidates: Vec<CandidateBody>,
    /// Less trustworthy than its words (a step or verdict was removed,
    /// candidates dropped for their evidence).
    pub degraded: bool,
    /// A newer diagnosis of the same incident, if any.
    #[schema(required = true)]
    pub superseded_by: Option<i64>,
}

fn diagnoses(state: &AppState) -> Result<&Diagnoses, ForgeError> {
    state.diagnoses.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "diagnoses_unconfigured")
    })
}

fn body(d: Diagnosis) -> Result<DiagnosisBody, ForgeError> {
    Ok(match d {
        Diagnosis::Shown(a) => {
            let candidates: Vec<CandidateBody> = serde_json::from_value(a.findings.clone())
                // clone: the stored row stays whole for the error context
                .map_err(|e| {
                    ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "llm_stored_value_invalid")
                        .with_context("column", "findings")
                        .with_context("advice_id", a.id)
                        .with_source(e)
                })?;
            DiagnosisBody {
                id: a.id,
                state: "shown".to_owned(),
                label: ADVICE_LABEL.to_owned(),
                input_class: a.input_class.as_str().to_owned(),
                note: None,
                model: Some(a.model),
                prompt_version: Some(a.prompt_version),
                created_at: Some(a.created_at),
                summary: Some(a.summary),
                candidates,
                degraded: a.degraded,
                superseded_by: a.superseded_by,
            }
        }
        Diagnosis::Withheld { id, input_class } => DiagnosisBody {
            id,
            state: "withheld".to_owned(),
            label: ADVICE_LABEL.to_owned(),
            input_class: input_class.as_str().to_owned(),
            note: Some(WITHHELD_NOTE.to_owned()),
            model: None,
            prompt_version: None,
            created_at: None,
            summary: None,
            candidates: Vec::new(),
            degraded: false,
            superseded_by: None,
        },
    })
}

/// `GET /api/v1/admin/diagnoses/{advice_id}` → `200`.
pub async fn one(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let audience = crate::routes::context::audience(&request);
    let path = request.raw_path().to_owned();
    let w = request.into_write();
    let built = async {
        let declared_agent = audience?.is_agent();
        let id = path
            .strip_prefix(PREFIX)
            .filter(|id| !id.contains('/'))
            .and_then(parse_positive_id)
            .ok_or_else(|| param_invalid("advice_id", "not_positive_decimal"))?;
        let read = diagnoses(&state)?.read(&w.proof, id, declared_agent);
        json(body(bounded(&state, "diagnosis_read", read).await?)?)
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}
