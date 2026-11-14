use super::*;
use crate::risk::RULES;

fn run(conclusion: CheckConclusion) -> Option<CheckRun> {
    CheckRun::new(CheckStatus::Completed, Some(conclusion))
}

fn clear_facts() -> Facts {
    Facts {
        mode: AutoMergeMode::Full,
        stop: None,
        draft: false,
        queue: QueueFacts::None,
        risk: RiskVerdict::Clear { warned: false },
        pipeline: PipelineState::None,
        blockers: Vec::new(),
        protected_path: false,
    }
}

fn risk_failure(rule: &str) -> RiskVerdict {
    let summary = format!("STOP: 1 stop, 0 warn [FG-REVIEW-0023].\ntarget main (1 changed file(s)):\n- stop {rule} x.sql:3 — reason [FG-REVIEW-0023]");
    deterministic_layer(run(CheckConclusion::Failure), Some(&summary))
}

#[test]
fn new_projects_are_full_and_existing_ones_keep_off() {
    assert_eq!(AutoMergeMode::NEW_PROJECT, AutoMergeMode::Full);
    assert_eq!(AutoMergeMode::EXISTING_PROJECT, AutoMergeMode::Off);
    for mode in AutoMergeMode::ALL {
        assert_eq!(AutoMergeMode::parse(mode.as_str()), Some(mode));
    }
    assert!(AutoMergeMode::Full.acts() && AutoMergeMode::Conditional.acts());
    assert!(!AutoMergeMode::Off.acts() && !AutoMergeMode::Reserve.acts());
    assert_eq!(AutoMergeMode::parse("FULL"), None);
}

#[test]
fn every_stop_rule_of_forge_risk_has_a_class_and_no_warn_rule_does() {
    for rule in RULES {
        let class = rule_class(rule.id);
        match rule.level {
            RiskLevel::Stop => assert!(class.is_some(), "{} has no class", rule.id),
            RiskLevel::Warn | RiskLevel::Ok => assert!(class.is_none(), "{} is no stop", rule.id),
        }
    }
    assert_eq!(rule_class("no_such_rule"), None);
}

/// FG-REVIEW-0026: each of the four classes (and an unassessable head)
/// stops auto-merge.
#[test]
fn each_stop_class_stops_auto_merge() {
    for (rule, class) in [
        ("mass_deletion_lines", StopClass::ProgramBreakage),
        ("sql_drop", StopClass::DataLoss),
        ("applied_migration_changed", StopClass::DataLoss),
        ("secret_in_content", StopClass::Leak),
        ("agent_config", StopClass::DeveloperPc),
        ("submodule_change", StopClass::DeveloperPc),
    ] {
        let facts = Facts {
            risk: risk_failure(rule),
            ..clear_facts()
        };
        assert_eq!(
            decide(&facts),
            Verdict::Stopped(Stop {
                class,
                reason: "risk_stop",
                detail: Some(rule.to_owned()),
            }),
            "{rule}"
        );
    }
    // A red required check: program breakage, named.
    let facts = Facts {
        blockers: vec![Blocker {
            reason: "required_check_not_successful",
            red_check: Some("ext/build".to_owned()),
        }],
        ..clear_facts()
    };
    assert_eq!(
        decide(&facts),
        Verdict::Stopped(Stop {
            class: StopClass::ProgramBreakage,
            reason: "required_check_failed",
            detail: Some("ext/build".to_owned()),
        })
    );
    // Its own pipeline red on the merge result.
    let facts = Facts {
        pipeline: PipelineState::Red,
        ..clear_facts()
    };
    assert!(matches!(
        decide(&facts),
        Verdict::Stopped(Stop {
            class: StopClass::ProgramBreakage,
            reason: "pipeline_failed",
            ..
        })
    ));
    // forge/risk errored: not assessed is a stop, never a pass.
    let facts = Facts {
        risk: deterministic_layer(run(CheckConclusion::Error), None),
        ..clear_facts()
    };
    assert!(matches!(
        decide(&facts),
        Verdict::Stopped(Stop {
            class: StopClass::NotAssessed,
            ..
        })
    ));
    // A failure whose summary names no rule forge knows: not assessed.
    assert_eq!(
        deterministic_layer(
            run(CheckConclusion::Failure),
            Some("- stop invented_rule x")
        ),
        RiskVerdict::Stop {
            class: StopClass::NotAssessed,
            rule: None
        }
    );
}

#[test]
fn a_warning_does_not_stop_auto_merge() {
    let facts = Facts {
        risk: deterministic_layer(
            run(CheckConclusion::Neutral),
            Some(
                "WARN: 0 stop, 1 warn.
- warn dependency_change Cargo.toml [FG-REVIEW-0024]",
            ),
        ),
        ..clear_facts()
    };
    assert_eq!(decide(&facts), Verdict::Enqueue { warned: true });
    assert_eq!(decide(&clear_facts()), Verdict::Enqueue { warned: false });
}

#[test]
fn the_worst_class_of_several_rules_is_shown() {
    let summary = "- stop mass_deletion_lines\n- stop secret_file a\n- stop sql_drop b:1";
    assert_eq!(
        stop_rules(summary),
        vec!["mass_deletion_lines", "secret_file", "sql_drop"]
    );
    assert_eq!(
        deterministic_layer(run(CheckConclusion::Failure), Some(summary)),
        RiskVerdict::Stop {
            class: StopClass::Leak,
            rule: Some("secret_file")
        }
    );
    // A warn line never counts as a stop rule.
    assert!(stop_rules("- warn new_migration m.sql").is_empty());
}

#[test]
fn the_ai_layer_can_only_raise() {
    let stop = risk_failure("sql_drop");
    for ai in [
        AiRisk::Ok,
        AiRisk::Warn,
        AiRisk::Unavailable,
        AiRisk::Stop(StopClass::Leak),
    ] {
        assert_eq!(combine(stop.clone(), Some(ai)), stop, "{ai:?}");
    }
    let clear = RiskVerdict::Clear { warned: false };
    assert_eq!(combine(clear.clone(), Some(AiRisk::Ok)), clear);
    assert_eq!(combine(clear.clone(), None), clear);
    assert_eq!(
        combine(clear.clone(), Some(AiRisk::Warn)),
        RiskVerdict::Clear { warned: true }
    );
    assert_eq!(
        combine(clear.clone(), Some(AiRisk::Stop(StopClass::DataLoss))),
        RiskVerdict::Stop {
            class: StopClass::DataLoss,
            rule: None
        }
    );
    // No answer from the AI is a stop, never an `ok`.
    assert!(matches!(
        combine(clear, Some(AiRisk::Unavailable)),
        RiskVerdict::Stop {
            class: StopClass::NotAssessed,
            ..
        }
    ));
}

#[test]
fn the_emergency_stop_comes_before_everything_but_the_mode() {
    let facts = Facts {
        stop: Some(StopScope::Instance),
        queue: QueueFacts::Live { auto: true },
        ..clear_facts()
    };
    assert!(matches!(
        decide(&facts),
        Verdict::Stopped(Stop {
            class: StopClass::EmergencyStop,
            reason: "instance_stopped",
            ..
        })
    ));
    let off = Facts {
        mode: AutoMergeMode::Reserve,
        stop: Some(StopScope::Project),
        ..clear_facts()
    };
    assert_eq!(decide(&off), Verdict::Inactive);
}

#[test]
fn what_it_waits_for_is_named_and_removals_by_people_hold_the_head() {
    let pending = Facts {
        risk: deterministic_layer(None, None),
        ..clear_facts()
    };
    assert_eq!(
        decide(&pending),
        Verdict::Waiting {
            reason: "risk_pending"
        }
    );
    let draft = Facts {
        draft: true,
        ..clear_facts()
    };
    assert_eq!(decide(&draft), Verdict::Waiting { reason: "draft" });
    let approvals = Facts {
        blockers: vec![Blocker {
            reason: "approvals_missing",
            red_check: None,
        }],
        ..clear_facts()
    };
    assert_eq!(
        decide(&approvals),
        Verdict::Waiting {
            reason: "approvals_missing"
        }
    );
    let held = Facts {
        queue: QueueFacts::Removed {
            reason: "dequeued".to_owned(),
        },
        ..clear_facts()
    };
    assert_eq!(
        decide(&held),
        Verdict::Held {
            reason: "dequeued_by_person"
        }
    );
    let red_alone = Facts {
        queue: QueueFacts::Removed {
            reason: "checks_failed".to_owned(),
        },
        ..clear_facts()
    };
    assert!(matches!(
        decide(&red_alone),
        Verdict::Stopped(Stop {
            class: StopClass::ProgramBreakage,
            reason: "merge_queue_checks_failed",
            ..
        })
    ));
    // Any other removal (a conflict, a new target tip) is decided again.
    let conflict = Facts {
        queue: QueueFacts::Removed {
            reason: "conflict".to_owned(),
        },
        ..clear_facts()
    };
    assert_eq!(decide(&conflict), Verdict::Enqueue { warned: false });
}

#[test]
fn conditional_keeps_protected_paths_for_a_person_and_full_does_not() {
    for path in [
        "db/migrations/0001_init.sql",
        ".forge/repo.yml",
        ".forge-ci.yml",
        "docs/CODEOWNERS",
        "src/auth/login.rs",
    ] {
        assert!(is_protected_path(path), "{path}");
    }
    assert!(!is_protected_path("src/lib.rs"));
    let conditional = Facts {
        mode: AutoMergeMode::Conditional,
        protected_path: true,
        ..clear_facts()
    };
    assert_eq!(
        decide(&conditional),
        Verdict::Waiting {
            reason: "protected_path"
        }
    );
    let full = Facts {
        mode: AutoMergeMode::Full,
        protected_path: true,
        ..clear_facts()
    };
    assert_eq!(decide(&full), Verdict::Enqueue { warned: false });
}

#[test]
fn the_pipeline_state_maps_its_run() {
    assert_eq!(PipelineState::of(None), PipelineState::None);
    assert_eq!(
        PipelineState::of(CheckRun::new(CheckStatus::InProgress, None)),
        PipelineState::Running
    );
    assert_eq!(
        PipelineState::of(run(CheckConclusion::Success)),
        PipelineState::Green
    );
    assert_eq!(
        PipelineState::of(run(CheckConclusion::Failure)),
        PipelineState::Red
    );
}

#[test]
fn the_audit_values_name_scope_and_modes() {
    let p = crate::ids::ProjectId::new(7);
    let change = AutoMergeChange::ModeChanged {
        project: p,
        from: AutoMergeMode::Off,
        to: AutoMergeMode::Full,
    };
    assert_eq!(change.action(), "auto_merge.mode_changed");
    assert_eq!(change.to_value()["to"], "full");
    let stop = AutoMergeChange::Stopped { project: None };
    assert_eq!(stop.to_value()["scope"], "instance");
    assert_eq!(stop.project(), None);
    for class in StopClass::ALL {
        assert_eq!(StopClass::parse(class.as_str()), Some(class));
    }
}

/// C9 security review: `neutral` also means "part of the head was not
/// assessed" (the content scan reads at most 60 files of 256 KiB). Such a
/// head, or a neutral run whose rules cannot be read, is never clear.
#[test]
fn a_partly_assessed_head_is_not_assessed_never_a_warning() {
    let partial = "WARN: 0 stop, 2 warn.
- warn dependency_change Cargo.toml
- warn content_not_scanned 61 file(s)";
    assert_eq!(
        deterministic_layer(run(CheckConclusion::Neutral), Some(partial)),
        RiskVerdict::Stop {
            class: StopClass::NotAssessed,
            rule: Some("content_not_scanned")
        }
    );
    for unreadable in [None, Some("WARN"), Some("- warn invented_rule x")] {
        assert_eq!(
            deterministic_layer(run(CheckConclusion::Neutral), unreadable),
            RiskVerdict::Stop {
                class: StopClass::NotAssessed,
                rule: None
            },
            "{unreadable:?}"
        );
    }
    for rule in NOT_ASSESSED_RULES {
        assert!(
            RULES
                .iter()
                .any(|r| r.id == *rule && r.level == RiskLevel::Warn),
            "{rule} is a warn rule of forge/risk"
        );
    }
}
