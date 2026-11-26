//! `gstn ops` (plan 19 L5-7b): operator reads. `ops diagnosis <id>` shows
//! an incident first diagnosis `forge diagnose` stored — advice only,
//! labelled as such; a confidential one is withheld from an agent's token.

use serde_json::Value;

use crate::app::{emit, EnvFn, Io};
use crate::cli::{Cli, OpsCommand};
use crate::error::{sanitize, Result};
use crate::hosting::{call, client, text};
use crate::http::Method;

/// The diagnosis as text: the label, the summary, each candidate with its
/// evidence (every server text sanitized: it came from a model).
pub(crate) fn diagnosis_text(out: &Value) -> String {
    let mut lines = vec![format!(
        "{} #{} ({}, {})",
        text(out, "label"),
        text(out, "id"),
        text(out, "state"),
        text(out, "input_class")
    )];
    if out["state"] == "withheld" {
        lines.push(text(out, "note"));
        return lines.join("\n");
    }
    lines.push(format!(
        "model {} / prompt v{} / {}{}",
        text(out, "model"),
        text(out, "prompt_version"),
        text(out, "created_at"),
        if out["degraded"] == Value::Bool(true) {
            " / 信頼度が低い"
        } else {
            ""
        }
    ));
    lines.push(format!("要約: {}", text(out, "summary")));
    let candidates: Vec<&Value> = out
        .get("candidates")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    if candidates.is_empty() {
        lines.push("原因の候補: なし".to_owned());
    }
    for (i, c) in candidates.into_iter().enumerate() {
        let at = match (c["path"].as_str(), c["line"].as_u64()) {
            (Some(p), Some(l)) => format!("{} L{l}", sanitize(p)),
            (Some(p), None) => sanitize(p),
            (None, _) => "-".to_owned(),
        };
        lines.push(format!(
            "{}. [{} {}] {}\n   根拠: {} 「{}」",
            i + 1,
            text(c, "severity"),
            text(c, "confidence"),
            text(c, "message"),
            at,
            text(c, "quote")
        ));
    }
    if let Some(newer) = out["superseded_by"].as_i64() {
        lines.push(format!("（新しい診断があります: #{newer}）"));
    }
    lines.join("\n")
}

/// `gstn ops`.
pub fn ops(cli: &Cli, cmd: &OpsCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    match cmd {
        OpsCommand::Diagnosis { id } => {
            let c = client(cli, env, io)?;
            let out = call(
                &c,
                Method::Get,
                &format!("/api/v1/admin/diagnoses/{id}"),
                &[],
                None,
            )?;
            let human = diagnosis_text(&out);
            emit(io, cli, out, &human)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_withheld_diagnosis_shows_no_words() {
        let out = serde_json::json!({
            "id": 4, "state": "withheld", "label": "AI の助言（提案）",
            "input_class": "confidential", "note": "機密の入力のため表示しません",
            "model": null, "prompt_version": null, "created_at": null, "summary": null,
            "candidates": [], "degraded": false, "superseded_by": null
        });
        let t = diagnosis_text(&out);
        assert!(t.contains("機密の入力のため表示しません"));
        assert!(!t.contains("要約"));
    }

    #[test]
    fn a_shown_diagnosis_lists_candidates_with_their_evidence() {
        let out = serde_json::json!({
            "id": 3, "state": "shown", "label": "AI の助言（提案）",
            "input_class": "internal", "note": null, "model": "qwen3:27b",
            "prompt_version": 1, "created_at": "2026-11-26T01:00:00Z",
            "summary": "プールが尽きた。", "degraded": true, "superseded_by": 9,
            "candidates": [{"severity": "high", "path": "log", "line": 2,
                "quote": "reason=acquire_timeout", "message": "書き込みのプール\u{1b}[31m",
                "confidence": 0.8}]
        });
        let t = diagnosis_text(&out);
        assert!(t.contains("AI の助言（提案） #3"));
        assert!(t.contains("log L2"));
        assert!(t.contains("信頼度が低い"));
        assert!(t.contains("#9"));
        assert!(!t.contains('\u{1b}'), "{t}");
    }
}
