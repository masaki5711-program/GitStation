#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The pure part of schedules: the draft's checks, the owner's decision on
//! the locked facts, and the plan of a scheduled run (never protected).

use forge_core::namespace_resolver::ProjectAccess;
use forge_core::{ProjectId, Role, Visibility};
use forge_db::access::Account;
use forge_db::projects::ProjectAccessFacts;

use super::*;

fn draft() -> ScheduleDraft {
    ScheduleDraft {
        description: "  nightly build ".to_owned(),
        branch: "main".to_owned(),
        cron: "0  2 * * *".to_owned(),
        utc_offset_minutes: 540,
        active: true,
    }
}

fn reason_of(e: &ForgeError) -> (String, String) {
    let get = |k: &str| {
        e.context()
            .iter()
            .find(|(key, _)| *key == k)
            .map(|(_, v)| v.to_string().trim_matches('"').to_owned())
            .unwrap_or_default()
    };
    (get("param"), get("reason"))
}

#[test]
fn a_draft_becomes_the_stored_input() {
    let input = draft().check().expect("valid");
    assert_eq!(input.description, "nightly build");
    assert_eq!(input.git_ref.as_str(), "refs/heads/main");
    assert_eq!(input.cron, "0 2 * * *", "spaces are normalized");
    let full = ScheduleDraft {
        branch: "refs/heads/release/1.0".to_owned(),
        ..draft()
    };
    assert_eq!(
        full.check().expect("valid").git_ref.as_str(),
        "refs/heads/release/1.0"
    );
}

#[test]
fn a_bad_draft_names_its_parameter_and_reason() {
    for (bad, param, reason) in [
        (
            ScheduleDraft {
                description: " ".to_owned(),
                ..draft()
            },
            "description",
            "empty",
        ),
        (
            ScheduleDraft {
                description: "x".repeat(256),
                ..draft()
            },
            "description",
            "too_long",
        ),
        (
            ScheduleDraft {
                description: "a\nb".to_owned(),
                ..draft()
            },
            "description",
            "control_character",
        ),
        (
            ScheduleDraft {
                branch: "a..b".to_owned(),
                ..draft()
            },
            "branch",
            "invalid_ref",
        ),
        (
            ScheduleDraft {
                branch: String::new(),
                ..draft()
            },
            "branch",
            "invalid_ref",
        ),
        (
            ScheduleDraft {
                cron: "* * * * *".to_owned(),
                ..draft()
            },
            "cron",
            "too_frequent",
        ),
        (
            ScheduleDraft {
                cron: "0 2 * *".to_owned(),
                ..draft()
            },
            "cron",
            "cron_field_count",
        ),
        (
            ScheduleDraft {
                cron: "0 0 31 2 *".to_owned(),
                ..draft()
            },
            "cron",
            "never_runs",
        ),
        (
            ScheduleDraft {
                utc_offset_minutes: 541,
                ..draft()
            },
            "utc_offset_minutes",
            "utc_offset_invalid",
        ),
    ] {
        let err = bad.check().expect_err(param);
        assert_eq!(err.code(), ErrorCode::RequestInvalid);
        assert_eq!(
            reason_of(&err),
            (param.to_owned(), reason.to_owned()),
            "{bad:?}"
        );
    }
}

fn facts(role: Option<Role>, active: bool, pending_deletion: bool) -> ProjectUpdateFacts {
    ProjectUpdateFacts {
        account: Account {
            active,
            is_admin: false,
            standing_admin: false,
        },
        project: ProjectAccessFacts {
            project: ProjectId::new(3),
            access: ProjectAccess {
                visibility: Visibility::Private,
                role,
                pending_deletion,
            },
        },
    }
}

/// The schedule runs with its owner's rights as they are now: a Developer
/// may; a Reporter, a stranger, a disabled account or a project being
/// deleted is refused with FG-CI-0026 carrying the refusal's code.
#[test]
fn a_schedule_whose_owner_lost_the_right_starts_nothing() {
    let owner = UserId::new(9);
    assert!(owner_decision(owner, &facts(Some(Role::Developer), true, false)).is_ok());
    assert!(owner_decision(owner, &facts(Some(Role::Maintainer), true, false)).is_ok());
    for (f, cause) in [
        (facts(Some(Role::Reporter), true, false), "FG-AUTH-0003"),
        (facts(None, true, false), "FG-REPO-0005"),
        (facts(Some(Role::Maintainer), false, false), "FG-AUTH-0004"),
        (facts(Some(Role::Maintainer), true, true), "FG-REPO-0006"),
    ] {
        let err = owner_decision(owner, &f).expect_err(cause);
        assert_eq!(err.code(), ErrorCode::ScheduleOwnerNotAllowed);
        assert!(
            err.context()
                .iter()
                .any(|(k, v)| *k == "cause_code" && v.to_string().contains(cause)),
            "{cause}: {:?}",
            err.context()
        );
    }
}

/// ADR-0021 amendment 2026-11-24 (review HIGH): a scheduled run has no
/// live session, so only the owner's standing (CLI-managed) administrator
/// flag counts — an IdP-granted flag gives no rights on a private project
/// the owner holds no role on.
#[test]
fn a_schedule_owner_counts_only_the_standing_admin_flag() {
    let owner = UserId::new(9);
    let mut idp = facts(None, true, false);
    idp.account.is_admin = true;
    let err = owner_decision(owner, &idp).expect_err("IdP flag");
    assert_eq!(err.code(), ErrorCode::ScheduleOwnerNotAllowed);
    let mut cli = facts(None, true, false);
    cli.account.is_admin = true;
    cli.account.standing_admin = true;
    assert!(owner_decision(owner, &cli).is_ok(), "the CLI flag counts");
}

#[test]
fn a_scheduled_run_on_a_protected_branch_is_not_protected() {
    let rules = forge_core::ProtectionRules::new(vec![forge_core::ProtectionRule {
        pattern: forge_core::RefPattern::parse("refs/heads/main").expect("pattern"),
        push_min_role: Role::Maintainer,
        merge: forge_core::MergeRule::default(),
    }]);
    let request = PipelineRequest {
        project: ProjectId::new(1),
        git_ref: RefName::parse("refs/heads/main").expect("ref"),
        sha: ObjectId::parse(&"b".repeat(40)).expect("sha"),
        source: Source::Schedule,
        merge_request: None,
        triggered_by: Some(UserId::new(9)),
    };
    assert!(!crate::protected_push(&request, &rules));
    let text = format!(
        "version: 1\nstages: [deploy]\njobs:\n  deploy:\n    stage: deploy\n    image: registry.internal/x@sha256:{}\n    timeout: 1m\n    script: [x]\n    secrets: [KEY]\n    rules:\n      branches: [main]\n      merge_requests: false\n",
        "a".repeat(64)
    );
    match Pipelines::plan(&request, text.as_bytes(), &rules)
        .expect("plan")
        .0
    {
        forge_db::pipelines::Planned::Invalid(problems) => {
            assert_eq!(problems[0].reason, "secret_on_unprotected_ref");
        }
        other @ (forge_db::pipelines::Planned::Jobs(_) | forge_db::pipelines::Planned::Error) => {
            panic!("a schedule never gets secrets: {other:?}")
        }
    }
}
