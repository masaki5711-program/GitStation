//! Kanflow's read-only tool pack (plan 19 §3.3 / W14-6, plan 02 §6 F-18,
//! ADR-0025 Amendment L5-9): `GET /api/v1/ai-tools` (`AiToolList`) lists
//! the tools kanflow's AI chat may offer its model, in kanflow's own form —
//! a name, a description and a JSON Schema of the arguments — with how each
//! call is made and a glossary of the state names (plan 08: the chat
//! answers people who do not know Git's words).
//!
//! Each tool is **one REST API v1 read route**, called by kanflow's
//! service account **acting for the person who asked** (`Forge-Act-As:
//! user:<id>`, no assertion: every tool's action is
//! `ActAsRule::ReadWithoutAssertion`). The route decides with
//! `authorize()` on the smaller of the two roles; what the person cannot
//! see is the same 404 and never reaches the answer. Every tool's answer
//! carries `as_of` (UTC, when it was read); numbers and lists come from
//! the tool, never from the model (plan 02 §6). An answer to a request
//! acting for a person is an agent audience's
//! (`Principal::is_agent_audience`): confidential files, work items and
//! the CODEOWNERS patterns naming them are withheld.
//!
//! The parity test (`ai_tools_tests.rs`) keeps every tool a registered
//! `GET` route whose action is a read kanflow may ask acting for a person.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::InstanceCtx;
use forge_errors::{ErrorCode, ForgeError};
use serde::Serialize;
use serde_json::{json, Value};

use crate::app::AppState;
use crate::registry::Dispatched;
use crate::routes::write::{json as to_json, no_store};

/// `GET` the tool pack.
pub const PATH: &str = "/api/v1/ai-tools";
/// The version of the contract (fields are only ever added within it).
pub const SCHEMA_VERSION: u32 = 1;
/// The header naming the person a call acts for.
pub const ACT_AS_HEADER: &str = "Forge-Act-As";

/// Now as RFC 3339 UTC (`2026-11-26T05:03:00Z`): every tool answer's
/// `as_of`, taken before the read (the answer is at least this fresh).
pub(crate) fn as_of_now() -> Result<String, ForgeError> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "clock_before_epoch")
        })?;
    Ok(forge_core::portal::rfc3339_utc(secs))
}

/// Where an argument goes on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ArgumentIn {
    Path,
    Query,
}

/// One argument's place in the request.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AiToolBindingBody {
    /// The argument's name (a property of `parameters`).
    pub name: String,
    #[serde(rename = "in")]
    pub location: ArgumentIn,
    /// The name on the wire (`{id}` of the path, a query key).
    pub wire: String,
}

/// One tool.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AiToolBody {
    /// The tool's name for the model (`snake_case`).
    pub name: String,
    /// What the model reads to choose the tool.
    pub description: String,
    /// Always `true`: the pack has no writes (plan 19 §3.3).
    pub read_only: bool,
    /// Always `GET`.
    pub method: String,
    /// As in `openapi.json`: `{wire}` for each path argument.
    pub path: String,
    /// The arguments as a JSON Schema object (kanflow's tool form).
    #[schema(value_type = Object)]
    pub parameters: Value,
    /// Where each argument goes.
    pub bindings: Vec<AiToolBindingBody>,
}

/// One state name and its plain words (plan 08).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AiTermBody {
    /// `merge_request_state`, `block_reason`, `pipeline_status`,
    /// `deployment_status`.
    pub kind: String,
    /// The name as the answers spell it.
    pub name: String,
    pub ja: String,
    pub en: String,
}

/// `GET /api/v1/ai-tools`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AiToolPackBody {
    pub schema_version: u32,
    /// Always `gitstation`: kanflow's routing table adds it as a pack.
    pub pack: String,
    /// How a tool is called: the service account's token, acting for the
    /// person who asked (`Forge-Act-As: user:<id>`, no assertion).
    pub act_as_header: String,
    pub tools: Vec<AiToolBody>,
    pub terms: Vec<AiTermBody>,
}

/// A static tool description.
pub(crate) struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub path: &'static str,
    /// `(name, in, wire, schema, required)`.
    pub args: &'static [(&'static str, ArgumentIn, &'static str, &'static str, bool)],
}

const PROJECT_ID: (&str, ArgumentIn, &str, &str, bool) = (
    "project_id",
    ArgumentIn::Path,
    "id",
    r#"{"type":"integer","minimum":1,"description":"The GitStation project id (the kanflow project's bound repository)."}"#,
    true,
);

/// Every tool of the pack (plan 19 §3.3: blocked reasons, pipelines,
/// deployments, a PBI's merge requests).
pub(crate) const TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "get_merge_request_blocked_reasons",
        description: "Why a merge request cannot be merged now: every blocking reason in its fixed order (the first is the one-line answer), with the approvals, required checks and discussions they come from, exactly as GitStation's merge decision sees them. Answers as_of.",
        path: "/api/v1/projects/{id}/merge-requests/{iid}/status",
        args: &[
            PROJECT_ID,
            (
                "iid",
                ArgumentIn::Path,
                "iid",
                r#"{"type":"integer","minimum":1,"description":"The merge request's number in the project (!<iid>)."}"#,
                true,
            ),
        ],
    },
    ToolDef {
        name: "list_merge_requests",
        description: "A project's merge requests, newest first; with work_item, only those linked to that PBI (its development status). Answers as_of.",
        path: "/api/v1/projects/{id}/merge-requests",
        args: &[
            PROJECT_ID,
            (
                "work_item",
                ArgumentIn::Query,
                "work_item",
                r#"{"type":"string","pattern":"^PBI-(0|[1-9][0-9]*)$","description":"A kanflow work item, PBI-<n>."}"#,
                false,
            ),
            (
                "state",
                ArgumentIn::Query,
                "state",
                r#"{"type":"string","enum":["open","merged","closed","all"],"description":"open (default), merged, closed or all."}"#,
                false,
            ),
            (
                "limit",
                ArgumentIn::Query,
                "limit",
                r#"{"type":"integer","minimum":1,"maximum":100,"description":"At most this many (default 20)."}"#,
                false,
            ),
        ],
    },
    ToolDef {
        name: "list_pipelines",
        description: "A project's CI pipelines, newest first: branch, commit, status (failed ones included) and error code. Metadata only, never logs. Answers as_of.",
        path: "/api/v1/projects/{id}/pipelines",
        args: &[
            PROJECT_ID,
            (
                "ref",
                ArgumentIn::Query,
                "ref",
                r#"{"type":"string","maxLength":255,"description":"Only this branch (main or refs/heads/main)."}"#,
                false,
            ),
            (
                "limit",
                ArgumentIn::Query,
                "limit",
                r#"{"type":"integer","minimum":1,"maximum":100,"description":"At most this many (default 20)."}"#,
                false,
            ),
        ],
    },
    ToolDef {
        name: "get_pipeline",
        description: "One CI pipeline with its jobs and each job's status. Metadata only, never logs. Answers as_of.",
        path: "/api/v1/projects/{id}/pipelines/{pipeline_id}",
        args: &[
            PROJECT_ID,
            (
                "pipeline_id",
                ArgumentIn::Path,
                "pipeline_id",
                r#"{"type":"integer","minimum":1,"description":"The pipeline's id (from list_pipelines)."}"#,
                true,
            ),
        ],
    },
    ToolDef {
        name: "list_deployments",
        description: "A project's deployments, newest first (at most 100): environment (dev, stg, prod), commit, status, when, and the error code of a failed one. Answers as_of.",
        path: "/api/v1/projects/{id}/deployments",
        args: &[PROJECT_ID],
    },
];

/// The plain words of every state name a tool answers (plan 08).
const TERMS: &[(&str, &str, &str, &str)] = &[
    ("merge_request_state", "open", "レビュー中", "open"),
    ("merge_request_state", "merged", "取り込み済み", "merged"),
    ("merge_request_state", "closed", "取り下げ", "closed"),
    ("block_reason", "not_open", "レビュー中ではない", "not open"),
    ("block_reason", "draft", "下書き", "draft"),
    (
        "block_reason",
        "merge_in_progress",
        "取り込みの最中",
        "merge in progress",
    ),
    (
        "block_reason",
        "source_branch_missing",
        "変更のブランチが無い",
        "source branch missing",
    ),
    (
        "block_reason",
        "mergeability_unknown",
        "取り込めるか確認中",
        "mergeability being checked",
    ),
    (
        "block_reason",
        "conflict",
        "ほかの変更とぶつかっている",
        "conflicts with other changes",
    ),
    (
        "block_reason",
        "ff_not_possible",
        "先に最新を取り込む必要がある",
        "needs the latest changes first",
    ),
    (
        "block_reason",
        "nothing_to_merge",
        "取り込む変更が無い",
        "nothing to merge",
    ),
    (
        "block_reason",
        "approvals_missing",
        "承認が足りない",
        "approvals missing",
    ),
    (
        "block_reason",
        "codeowner_approval_missing",
        "担当者の承認が足りない",
        "code owner approval missing",
    ),
    (
        "block_reason",
        "codeowners_invalid",
        "担当者の設定に誤り",
        "code owners file invalid",
    ),
    (
        "block_reason",
        "required_check_missing",
        "必須の検査がまだ無い",
        "required check not reported",
    ),
    (
        "block_reason",
        "required_check_stale",
        "必須の検査が古い",
        "required check out of date",
    ),
    (
        "block_reason",
        "required_check_not_successful",
        "必須の検査が成功していない",
        "required check not successful",
    ),
    (
        "block_reason",
        "unresolved_discussions",
        "未解決の指摘がある",
        "unresolved discussions",
    ),
    (
        "block_reason",
        "work_item_link_missing",
        "PBI と結びついていない",
        "no linked work item",
    ),
    (
        "block_reason",
        "config_change_needs_maintainer_approval",
        "設定の変更に管理者の承認が要る",
        "configuration change needs a maintainer's approval",
    ),
    (
        "block_reason",
        "changed_paths_unknown",
        "変更したファイルを確かめられない",
        "changed files could not be read",
    ),
    ("pipeline_status", "pending", "順番待ち", "waiting"),
    ("pipeline_status", "running", "実行中", "running"),
    ("pipeline_status", "succeeded", "成功", "succeeded"),
    ("pipeline_status", "failed", "失敗", "failed"),
    ("pipeline_status", "canceled", "取り消し", "canceled"),
    ("pipeline_status", "skipped", "実行しなかった", "skipped"),
    (
        "pipeline_status",
        "invalid",
        "定義に誤り",
        "invalid definition",
    ),
    (
        "pipeline_status",
        "error",
        "読み込めなかった",
        "could not be read",
    ),
    (
        "deployment_status",
        "pending_approval",
        "承認待ち",
        "waiting for approval",
    ),
    ("deployment_status", "queued", "順番待ち", "queued"),
    ("deployment_status", "running", "配備中", "deploying"),
    ("deployment_status", "succeeded", "成功", "succeeded"),
    ("deployment_status", "failed", "失敗", "failed"),
    (
        "deployment_status",
        "rolled_back",
        "元に戻した",
        "rolled back",
    ),
    (
        "deployment_status",
        "rollback_failed",
        "元に戻せなかった",
        "rollback failed",
    ),
    ("deployment_status", "blocked", "止められた", "blocked"),
];

fn tool_body(t: &ToolDef) -> Result<AiToolBody, ForgeError> {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    let mut bindings = Vec::new();
    for (name, location, wire, schema, needed) in t.args {
        let schema: Value = serde_json::from_str(schema).map_err(|_| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "ai_tool_schema_invalid")
                .with_context("tool", t.name)
                .with_context("argument", *name)
        })?;
        properties.insert((*name).to_owned(), schema);
        if *needed {
            required.push(Value::from(*name));
        }
        bindings.push(AiToolBindingBody {
            name: (*name).to_owned(),
            location: *location,
            wire: (*wire).to_owned(),
        });
    }
    Ok(AiToolBody {
        name: t.name.to_owned(),
        description: t.description.to_owned(),
        read_only: true,
        method: "GET".to_owned(),
        path: t.path.to_owned(),
        parameters: json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        }),
        bindings,
    })
}

/// The whole pack.
pub(crate) fn pack() -> Result<AiToolPackBody, ForgeError> {
    Ok(AiToolPackBody {
        schema_version: SCHEMA_VERSION,
        pack: "gitstation".to_owned(),
        act_as_header: ACT_AS_HEADER.to_owned(),
        tools: TOOLS.iter().map(tool_body).collect::<Result<_, _>>()?,
        terms: TERMS
            .iter()
            .map(|(kind, name, ja, en)| AiTermBody {
                kind: (*kind).to_owned(),
                name: (*name).to_owned(),
                ja: (*ja).to_owned(),
                en: (*en).to_owned(),
            })
            .collect(),
    })
}

/// `GET /api/v1/ai-tools` → `200` the pack.
pub async fn list(_state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    match pack().and_then(to_json) {
        Ok(body) => no_store(StatusCode::OK, body),
        Err(e) => request.ctx().fail(e).into_response(),
    }
}

#[cfg(test)]
#[path = "ai_tools_tests.rs"]
mod tests;
