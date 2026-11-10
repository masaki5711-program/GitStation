//! Unit tests of the merge request routes' parsing (branch names, ids,
//! the list query).

use forge_core::{MergeRequestState, ProjectId};
use forge_errors::{ErrorCode, ForgeError};

use super::loaders::{branch_param, ids, version_ids};
use super::{page_query, DEFAULT_LIMIT};

fn rule(err: &ForgeError) -> String {
    err.to_string()
}

#[test]
fn branch_params_are_branches_only() {
    assert_eq!(
        branch_param("source_branch", "feature/x")
            .expect("short")
            .as_str(),
        "refs/heads/feature/x"
    );
    assert_eq!(
        branch_param("source_branch", "refs/heads/main")
            .expect("full")
            .as_str(),
        "refs/heads/main"
    );
    for (raw, why) in [
        ("refs/tags/v1", "not_a_branch"),
        ("refs/merge-requests/1/head", "not_a_branch"),
        ("-rf", "invalid_ref_name"),
        ("a..b", "invalid_ref_name"),
        ("", "invalid_ref_name"),
    ] {
        let err = branch_param("target_branch", raw).expect_err(raw);
        assert_eq!(err.code(), ErrorCode::RequestInvalid, "{raw}");
        assert!(rule(&err).contains(why), "{raw}: {err}");
    }
}

#[test]
fn ids_and_queries_are_strict() {
    let p = "/api/v1/projects/7/merge-requests/3";
    assert_eq!(ids(p, "").expect("ids"), (ProjectId::new(7), 3));
    assert_eq!(
        ids("/api/v1/projects/7/merge-requests/3/close", "/close").expect("ids"),
        (ProjectId::new(7), 3)
    );
    for bad in [
        "/api/v1/projects/7/merge-requests/0",
        "/api/v1/projects/7/merge-requests/03",
        "/api/v1/projects/7/merge-requests/3/x",
        "/api/v1/projects/x/merge-requests/3",
    ] {
        assert!(ids(bad, "").is_err(), "{bad}");
    }
    assert_eq!(
        page_query(None).expect("defaults"),
        (Some(MergeRequestState::Open), DEFAULT_LIMIT, None)
    );
    assert_eq!(
        page_query(Some("state=all&limit=5&before=9")).expect("all"),
        (None, 5, Some(9))
    );
    for bad in [
        "state=draft",
        "limit=101",
        "limit=0",
        "before=-1",
        "x=1",
        "limit=1&limit=2",
    ] {
        assert!(page_query(Some(bad)).is_err(), "{bad}");
    }
}

/// M3 R2: `…/{iid}/versions/{version}/diff` — both numbers plain positive
/// decimals, nothing after `/diff`.
#[test]
fn version_diff_paths_name_an_iid_and_a_version() {
    assert_eq!(
        version_ids("/api/v1/projects/7/merge-requests/3/versions/2/diff").expect("ids"),
        (ProjectId::new(7), 3, 2)
    );
    for (raw, param) in [
        (
            "/api/v1/projects/7/merge-requests/3/versions/0/diff",
            "version",
        ),
        (
            "/api/v1/projects/7/merge-requests/3/versions/02/diff",
            "version",
        ),
        (
            "/api/v1/projects/7/merge-requests/3/versions/x/diff",
            "version",
        ),
        (
            "/api/v1/projects/7/merge-requests/3/versions/2/diff/x",
            "version",
        ),
        (
            "/api/v1/projects/7/merge-requests/3/versions/2/3/diff",
            "version",
        ),
        ("/api/v1/projects/7/merge-requests/a/versions/2/diff", "iid"),
        ("/api/v1/projects/x/merge-requests/3/versions/2/diff", "id"),
    ] {
        let err = version_ids(raw).expect_err(raw);
        assert_eq!(err.code(), ErrorCode::RequestInvalid, "{raw}");
        assert!(rule(&err).contains(param), "{raw}: {err}");
    }
}

// ── U2: the merge status contract in the OpenAPI document ──

fn openapi() -> serde_json::Value {
    let doc = crate::generated_documents("test")
        .into_iter()
        .find(|(path, _)| path.ends_with("openapi.json"))
        .map(|(_, body)| body)
        .expect("the OpenAPI document is generated");
    serde_json::from_str(&doc).expect("OpenAPI is JSON")
}

fn schema<'a>(doc: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    &doc["components"]["schemas"][name]
}

/// One value of every `BlockReason` variant (`StatusReasonName::of` is
/// exhaustive, so a new variant fails to build there first; this list
/// then fails the count below).
fn every_block_reason() -> Vec<forge_review::BlockReason> {
    use forge_review::merges::evaluate::CheckStatus;
    use forge_review::BlockReason as B;
    let check = forge_core::CheckName::parse("ci/gates").expect("check name");
    vec![
        B::NotOpen,
        B::Draft,
        B::MergeInProgress,
        B::SourceBranchMissing,
        B::MergeabilityUnknown,
        B::Conflict,
        B::FfNotPossible,
        B::NothingToMerge,
        B::ApprovalsMissing {
            rule: "@protected_branch".to_owned(),
            required: 1,
            counted: 0,
        },
        B::CodeownerApprovalMissing {
            pattern: "*".to_owned(),
            owners: vec!["@olga".to_owned()],
        },
        B::CodeownersInvalid {
            line: 1,
            reason: "syntax",
        },
        B::RequiredCheckMissing {
            name: check.clone(),
        },
        B::RequiredCheckStale {
            name: check.clone(),
        },
        B::RequiredCheckNotSuccessful {
            name: check,
            status: CheckStatus::Queued,
            conclusion: None,
        },
        B::UnresolvedDiscussions { count: 1 },
        B::WorkItemLinkMissing,
        B::ConfigChangeNeedsMaintainerApproval,
        B::ChangedPathsUnknown { reason: "budget" },
    ]
}

/// `reasons[].reason` is an OpenAPI enum of exactly the names
/// `BlockReason::as_str` produces, and `docs/api/merge-status.md` lists
/// each of them.
#[test]
fn status_reason_schema_lists_every_block_reason() {
    use super::StatusReasonName;
    let reasons = every_block_reason();
    let names: std::collections::BTreeSet<&str> = reasons.iter().map(|r| r.as_str()).collect();
    assert_eq!(names.len(), StatusReasonName::ALL.len(), "one per variant");
    for r in &reasons {
        let named = serde_json::to_value(StatusReasonName::of(r)).expect("name");
        assert_eq!(named, r.as_str(), "{r:?}");
    }
    let all: std::collections::BTreeSet<String> = StatusReasonName::ALL
        .iter()
        .map(|n| {
            serde_json::to_value(n)
                .expect("name")
                .as_str()
                .expect("a string")
                .to_owned()
        })
        .collect();
    assert_eq!(
        all,
        names.iter().map(|n| (*n).to_owned()).collect(),
        "ALL is every name"
    );
    let doc = openapi();
    let reason = &schema(&doc, "StatusReasonBody")["properties"]["reason"];
    let target = reason["$ref"]
        .as_str()
        .and_then(|r| r.rsplit('/').next())
        .unwrap_or_else(|| panic!("reason refers to its enum: {reason}"));
    let listed: std::collections::BTreeSet<String> = schema(&doc, target)["enum"]
        .as_array()
        .unwrap_or_else(|| panic!("{target} is an enum"))
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    assert_eq!(listed, all, "the OpenAPI enum");
    let contract = include_str!("../../../../../docs/api/merge-status.md");
    for name in &all {
        assert!(
            contract.contains(&format!("| `{name}` |")),
            "docs/api/merge-status.md lists {name}"
        );
    }
}

/// `schema_version` 1 only grows: every field of the contract as first
/// published (M3 R6) is still there and still required; U2 added `viewer`.
#[test]
fn status_schema_v1_fields_are_only_added() {
    assert_eq!(super::status::SCHEMA_VERSION, 1);
    let doc = openapi();
    let required = |name: &str| -> std::collections::BTreeSet<String> {
        schema(&doc, name)["required"]
            .as_array()
            .unwrap_or_else(|| panic!("{name} lists its required fields"))
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    };
    let v1: &[(&str, &[&str])] = &[
        (
            "MergeRequestStatusBody",
            &[
                "schema_version",
                "iid",
                "head_sha",
                "target_branch",
                "method",
                "mergeable",
                "reasons",
                "approvals",
                "checks",
                "discussions",
                "codeowners",
            ],
        ),
        ("StatusReasonBody", &["reason", "detail"]),
        ("StatusApprovalsBody", &["rules", "approvals", "codeowners"]),
        (
            "StatusRuleBody",
            &["name", "applies", "required", "counted"],
        ),
        (
            "StatusApprovalBody",
            &["approver_id", "username", "counted", "uncounted_reason"],
        ),
        ("StatusCodeownerBody", &["pattern", "owners", "satisfied"]),
        ("StatusCheckBody", &["name", "status", "conclusion"]),
        ("StatusDiscussionsBody", &["unresolved"]),
        ("StatusCodeownersBody", &["enforced", "suspended_until"]),
    ];
    for (name, fields) in v1 {
        let now = required(name);
        for field in *fields {
            assert!(
                now.contains(*field),
                "{name}.{field} was removed or made optional"
            );
        }
    }
    let top = required("MergeRequestStatusBody");
    assert!(top.contains("viewer"), "U2 adds viewer");
    assert!(top.contains("work_item_link"), "I1d adds work_item_link");
    assert_eq!(
        required("StatusWorkItemLinkBody"),
        ["linked"].into_iter().map(str::to_owned).collect()
    );
    assert_eq!(
        required("StatusViewerBody"),
        [
            "approved_by_me",
            "can_approve",
            "can_mark_ready",
            "can_merge",
            "can_unapprove"
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
}
