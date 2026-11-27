//! `gstn llm …` (plan 19 §8.2, ADR-0039): a project's `llm.review` (read;
//! changing it is a Maintainer's in the browser) and the gateway's pause
//! and resume (an administrator; a token needs `api` and `admin`). One REST
//! call each; the ids are validated by clap and only formatted into the
//! path.

use serde_json::Value;

use crate::app::{emit, EnvFn, Io};
use crate::cli::{Cli, LlmCommand};
use crate::error::Result;
use crate::hosting::{call, client, text};
use crate::http::Method;

/// A project's `llm.review` as text.
fn review_text(s: &Value) -> String {
    let changed = match s.get("changed_at").and_then(Value::as_str) {
        Some(_) => format!(" (changed {})", text(s, "changed_at")),
        None => " (never changed: the default)".to_owned(),
    };
    format!("llm.review: {}{changed}", text(s, "review"))
}

/// The gateway's pause as text.
fn gateway_text(g: &Value) -> String {
    let state = if g.get("paused").and_then(Value::as_bool) == Some(true) {
        "paused"
    } else {
        "running"
    };
    let unchanged = if text(g, "outcome") == "unchanged" {
        " (it was so already)"
    } else {
        ""
    };
    format!("LLM gateway {state}{unchanged}; the worker follows within 10 s")
}

pub fn llm(cli: &Cli, cmd: &LlmCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    let c = client(cli, env, io)?;
    match cmd {
        LlmCommand::Review { project } => {
            let path = format!("/api/v1/projects/{project}/llm");
            let out = call(&c, Method::Get, &path, &[], None)?;
            let human = review_text(&out);
            emit(io, cli, out, &human)
        }
        LlmCommand::Pause | LlmCommand::Resume => {
            let verb = if matches!(cmd, LlmCommand::Pause) {
                "pause"
            } else {
                "resume"
            };
            let path = format!("/api/v1/admin/llm/{verb}");
            let out = call(&c, Method::Post, &path, &[], None)?;
            let human = gateway_text(&out);
            emit(io, cli, out, &human)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_setting_and_the_pause_read_as_one_line() {
        assert_eq!(
            review_text(&json!({"review": "off", "changed_at": null})),
            "llm.review: off (never changed: the default)"
        );
        assert_eq!(
            review_text(&json!({"review": "advisory", "changed_at": "2026-11-28T00:00:00Z"})),
            "llm.review: advisory (changed 2026-11-28T00:00:00Z)"
        );
        assert_eq!(
            gateway_text(&json!({"paused": true, "outcome": "changed"})),
            "LLM gateway paused; the worker follows within 10 s"
        );
        assert_eq!(
            gateway_text(&json!({"paused": false, "outcome": "unchanged"})),
            "LLM gateway running (it was so already); the worker follows within 10 s"
        );
    }
}
