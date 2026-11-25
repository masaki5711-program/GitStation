//! `forge doctor` for the local LLM gateway (plan 19 §8.4, ADR-0039;
//! runbooks llm-gateway, llm-model-update): `llm.gateway`, no database.
//!
//! - skipped while `llm.enabled` is off (the default; production until a
//!   GPU host is named);
//! - Ollama cannot be asked (`/api/tags`, 5 s): `FG-LLM-0001` with `reason`;
//! - the model's digest is not `llm.model`'s (or the model is missing):
//!   `FG-LLM-0003` (alert);
//! - otherwise ok, with the model and whether it is paused.
//!
//! Never part of `/readyz`: the LLM only advises (plan 19 §8.4).

use std::time::Duration;

use forge_core::config::Config;
use forge_errors::{ErrorCode, ForgeError};
use forge_llm::ollama::OllamaProvider;
use forge_llm::ModelCatalog;

use super::{fail, ok, skip, Check};

pub(super) const NAME: &str = "llm.gateway";

/// `llm.gateway`.
pub(super) async fn gateway_check(cfg: &Config) -> Check {
    let llm = &cfg.llm;
    let (true, Some(endpoint), Some(model)) =
        (llm.enabled, llm.endpoint.as_deref(), llm.model.as_ref())
    else {
        return skip(NAME, "llm.enabled is off: no LLM advice is made");
    };
    let ca = match llm.ca_pem() {
        Ok(ca) => ca,
        Err(e) => return fail(NAME, e),
    };
    let provider = match OllamaProvider::new(endpoint, ca.as_deref(), Duration::from_secs(5)) {
        Ok(p) => p,
        Err(e) => return fail(NAME, e),
    };
    match provider.digest(&model.name).await {
        Ok(Some(d)) if model.digest_matches(&d) => ok(
            NAME,
            &format!(
                "{endpoint}: {model} present{}",
                if llm.paused { " (paused)" } else { "" }
            ),
        ),
        Ok(found) => {
            let e = ForgeError::new(ErrorCode::LlmModelDigestMismatch)
                .with_context("model", &model.name)
                .with_context("expected_digest", &model.digest);
            fail(
                NAME,
                match found {
                    Some(d) => e.with_context("actual_digest", d),
                    None => e.with_context("reason", "model_missing"),
                },
            )
        }
        Err(e) => fail(NAME, e.with_context("endpoint", endpoint)),
    }
}
