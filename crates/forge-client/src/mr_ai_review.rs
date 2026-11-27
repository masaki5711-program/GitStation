//! `gstn mr ai-review` (M5 L5-5, plan 19 §3.1): the local LLM's first pass
//! of a merge request, as advice — `GET /api/v1/projects/{id}/
//! merge-requests/{iid}/ai-review`. The project's `llm.review`, the
//! `forge/ai-review` run on the head and the newest advice (summary,
//! findings with their evidence, the deep-review mark). A confidential
//! advice is withheld from an agent token ("機密の入力のため表示しません").
//! The advice never decides a merge.

use serde_json::Value;

use crate::app::{emit, EnvFn, Io};
use crate::cli::Cli;
use crate::error::{sanitize, Result};
use crate::hosting::{call, client, text};
use crate::http::Method;

/// `gstn mr ai-review --project ID --iid N`.
pub fn ai_review(
    cli: &Cli,
    (project, iid): (i64, i64),
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    let c = client(cli, env, io)?;
    let path = format!("/api/v1/projects/{project}/merge-requests/{iid}/ai-review");
    let body = call(&c, Method::Get, &path, &[], None)?;
    let h = human(&body);
    emit(io, cli, body, &h)
}

/// `gstn mr ai-review-rerun --project ID --iid N` (plan 19 §8.2): `POST
/// …/ai-review/rerun`. `queued` (the worker asks the head again on its next
/// pass) or `pending` (it still waits: nothing counted); 409 `FG-LLM-0008`
/// when closed or off, 429 `FG-WEB-0010` past 3 an hour.
pub fn rerun(cli: &Cli, (project, iid): (i64, i64), env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    let c = client(cli, env, io)?;
    let path = format!("/api/v1/projects/{project}/merge-requests/{iid}/ai-review/rerun");
    let body = call(&c, Method::Post, &path, &[], None)?;
    let h = rerun_human(&body);
    emit(io, cli, body, &h)
}

pub(crate) fn rerun_human(body: &Value) -> String {
    match body.get("outcome").and_then(Value::as_str) {
        Some("queued") => format!(
            "!{}: the AI first pass of {} is asked again ({} left this hour)",
            text(body, "iid"),
            text(body, "head_sha"),
            text(body, "remaining")
        ),
        _ => format!(
            "!{}: {} still waits for its AI first pass (nothing asked again)",
            text(body, "iid"),
            text(body, "head_sha")
        ),
    }
}

/// The mode and the run, then the advice with its label (server text
/// sanitized).
pub(crate) fn human(body: &Value) -> String {
    let check = body.get("check").unwrap_or(&Value::Null);
    let run = if check.is_null() {
        "no forge/ai-review run".to_owned()
    } else {
        match check.get("conclusion").and_then(Value::as_str) {
            Some(c) => format!("forge/ai-review {}", sanitize(c)),
            None => format!("forge/ai-review {}", text(check, "status")),
        }
    };
    let mut out = format!(
        "!{}: llm.review {}; {run}\nhead: {}",
        text(body, "iid"),
        text(body, "mode"),
        text(body, "head_sha")
    );
    let advice = body.get("advice").unwrap_or(&Value::Null);
    out.push_str(&format!("\n{}", text(advice, "label")));
    match advice.get("state").and_then(Value::as_str) {
        Some("shown") => {
            out.push_str(&format!(
                " — model {}, prompt v{}, {}",
                text(advice, "model"),
                text(advice, "prompt_version"),
                text(advice, "created_at")
            ));
            if advice.get("stale") == Some(&Value::Bool(true)) {
                out.push_str(&format!(
                    "\n(stale: made for head {})",
                    text(advice, "head_sha")
                ));
            }
            if advice.get("degraded") == Some(&Value::Bool(true)) {
                out.push_str("\n(low confidence: the answer was cleaned by code)");
            }
            out.push_str(&format!("\n{}", text(advice, "summary")));
            if let Some(d) = advice.get("deep_review").and_then(Value::as_str) {
                out.push_str(&format!("\ndeep review: {}", sanitize(d)));
            }
            for f in advice
                .get("findings")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                let place = match (
                    f.get("path").and_then(Value::as_str),
                    f.get("line").and_then(Value::as_i64),
                ) {
                    (Some(p), Some(l)) => format!(" {}:{l}", sanitize(p)),
                    (Some(p), None) => format!(" {}", sanitize(p)),
                    _ => String::new(),
                };
                out.push_str(&format!(
                    "\n- {}{place} — {}\n  quote: {}",
                    text(f, "severity"),
                    text(f, "message"),
                    text(f, "quote")
                ));
            }
        }
        _ => out.push_str(&format!("\n{}", text(advice, "note"))),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_human_form_labels_the_advice_and_says_when_there_is_none() {
        let none = serde_json::json!({
            "iid": 3, "mode": "off", "head_sha": "a", "check": null,
            "advice": {"state": "none", "label": "AI の助言（提案）", "note": "AI の助言は今ありません"}
        });
        let h = human(&none);
        assert!(h.contains("llm.review off; no forge/ai-review run"));
        assert!(h.contains("AI の助言は今ありません"));
        let shown = serde_json::json!({
            "iid": 3, "mode": "advisory", "head_sha": "a",
            "check": {"status": "completed", "conclusion": "neutral"},
            "advice": {"state": "shown", "label": "AI の助言（提案）", "model": "qwen3:27b",
                       "prompt_version": 1, "created_at": "2026-11-26T00:00:00Z", "stale": true,
                       "head_sha": "b", "summary": "Reads the size.", "deep_review": "a migration",
                       "findings": [{"severity": "high", "path": "src/a.rs", "line": 4,
                                     "quote": "unwrap()", "message": "panics"}]}
        });
        let h = human(&shown);
        assert!(h.contains("forge/ai-review neutral"));
        assert!(h.contains("model qwen3:27b, prompt v1"));
        assert!(h.contains("(stale: made for head b)"));
        assert!(h.contains("deep review: a migration"));
        assert!(h.contains("- high src/a.rs:4 — panics"));
    }

    #[test]
    fn the_rerun_says_whether_it_was_asked_again() {
        let queued =
            serde_json::json!({"iid": 3, "outcome": "queued", "head_sha": "a", "remaining": 2});
        assert_eq!(
            rerun_human(&queued),
            "!3: the AI first pass of a is asked again (2 left this hour)"
        );
        let pending =
            serde_json::json!({"iid": 3, "outcome": "pending", "head_sha": "a", "remaining": null});
        assert_eq!(
            rerun_human(&pending),
            "!3: a still waits for its AI first pass (nothing asked again)"
        );
    }
}
