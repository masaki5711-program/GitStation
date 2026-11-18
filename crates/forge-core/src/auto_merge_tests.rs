use super::*;
use crate::risk::RULES;

fn run(conclusion: CheckConclusion) -> Option<CheckRun> {
    CheckRun::new(CheckStatus::Completed, Some(conclusion))
}

fn clear_facts() -> Facts {
    Facts {
        mode: AutoMergeMode::Full,
        full_allowed: true,
        stop: None,
        draft: false,
        queue: QueueFacts::None,
        risk: RiskVerdict::Clear { warned: false },
        pipeline: PipelineState::None,
        doc_lint: DocLintState::Clear,
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
    // Plan 18 S1: the train lint's removals keep the same head away too.
    let doc_lint = Facts {
        queue: QueueFacts::Removed {
            reason: crate::train_lint::FAILED_REASON.to_owned(),
        },
        ..clear_facts()
    };
    assert_eq!(
        decide(&doc_lint),
        Verdict::Stopped(Stop {
            class: StopClass::ProgramBreakage,
            reason: "merge_queue_doc_lint_failed",
            detail: Some("forge/doc-lint".to_owned()),
        })
    );
    let strict_base = Facts {
        queue: QueueFacts::Removed {
            reason: crate::train_lint::BASE_NOT_CLEAN_REASON.to_owned(),
        },
        ..clear_facts()
    };
    assert_eq!(
        decide(&strict_base),
        Verdict::Held {
            reason: "merge_queue_doc_lint_base"
        }
    );
    // Strict only because `.forge/repo.yml` is not valid: the config is
    // named, and it is held for a person as well.
    let config = Facts {
        queue: QueueFacts::Removed {
            reason: crate::train_lint::REPO_CONFIG_INVALID_REASON.to_owned(),
        },
        ..clear_facts()
    };
    assert_eq!(
        decide(&config),
        Verdict::Held {
            reason: "merge_queue_repo_config_invalid"
        }
    );
    // Not lintable three times: not assessed, never enqueued again by
    // itself (a `stalled` removal is — a late test may pass the next time).
    let unlintable = Facts {
        queue: QueueFacts::Removed {
            reason: crate::train_lint::UNAVAILABLE_REASON.to_owned(),
        },
        ..clear_facts()
    };
    assert_eq!(
        decide(&unlintable),
        Verdict::Stopped(Stop {
            class: StopClass::NotAssessed,
            reason: "merge_queue_doc_lint_unavailable",
            detail: Some("forge/doc-lint".to_owned()),
        })
    );
    let stalled = Facts {
        queue: QueueFacts::Removed {
            reason: "stalled".to_owned(),
        },
        ..clear_facts()
    };
    assert_eq!(decide(&stalled), Verdict::Enqueue { warned: false });
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

/// Plan 17 §1.7 (P17-3): raw data in an AI evaluation case stops auto-merge
/// as a leak (no new class); an AI asset change only warns.
#[test]
fn ai_case_raw_data_is_a_leak_stop_and_ai_asset_only_warns() {
    assert_eq!(rule_class("ai_case_raw_data"), Some(StopClass::Leak));
    assert_eq!(rule_class("ai_asset"), None);
    let facts = Facts {
        risk: risk_failure("ai_case_raw_data"),
        ..clear_facts()
    };
    assert_eq!(
        decide(&facts),
        Verdict::Stopped(Stop {
            class: StopClass::Leak,
            reason: "risk_stop",
            detail: Some("ai_case_raw_data".to_owned()),
        })
    );
    // A secret beside it still shows the same class.
    let summary = "STOP: 2 stop, 1 warn.\n- stop ai_case_raw_data evals/cases/a/b.json:3 — r\n- stop agent_config .claude/x — r\n- warn ai_asset evals/cases/a/b.json — r";
    assert_eq!(
        deterministic_layer(run(CheckConclusion::Failure), Some(summary)),
        RiskVerdict::Stop {
            class: StopClass::Leak,
            rule: Some("ai_case_raw_data"),
        }
    );
    let warned = "WARN: 0 stop, 1 warn [FG-REVIEW-0024].\n- warn ai_asset skills/pe-x.md — r [FG-REVIEW-0024]";
    let risk = deterministic_layer(run(CheckConclusion::Neutral), Some(warned));
    assert_eq!(risk, RiskVerdict::Clear { warned: true });
    let facts = Facts {
        risk,
        ..clear_facts()
    };
    assert_eq!(decide(&facts), Verdict::Enqueue { warned: true });
    assert!(!NOT_ASSESSED_RULES.contains(&"ai_asset"));
}

/// M4 close: without the instance's `merge.allow_full_auto`, `full` is
/// `reserve` in effect and its merge requests are held with the reason;
/// `conditional` still acts; an emergency stop is still shown first.
#[test]
fn full_without_the_instance_allowance_is_held_and_conditional_acts() {
    assert_eq!(AutoMergeMode::Full.effective(false), AutoMergeMode::Reserve);
    assert_eq!(AutoMergeMode::Full.effective(true), AutoMergeMode::Full);
    for mode in [
        AutoMergeMode::Off,
        AutoMergeMode::Reserve,
        AutoMergeMode::Conditional,
    ] {
        assert_eq!(mode.effective(false), mode);
    }
    let held = Facts {
        full_allowed: false,
        queue: QueueFacts::Live { auto: true },
        ..clear_facts()
    };
    assert_eq!(
        decide(&held),
        Verdict::Held {
            reason: "full_auto_not_allowed"
        }
    );
    let conditional = Facts {
        mode: AutoMergeMode::Conditional,
        full_allowed: false,
        ..clear_facts()
    };
    assert_eq!(decide(&conditional), Verdict::Enqueue { warned: false });
    let stopped = Facts {
        full_allowed: false,
        stop: Some(StopScope::Project),
        ..clear_facts()
    };
    assert!(matches!(
        decide(&stopped),
        Verdict::Stopped(Stop {
            class: StopClass::EmergencyStop,
            ..
        })
    ));
    let change = AutoMergeChange::FullAutoAllowed { allowed: false };
    assert_eq!(change.action(), "auto_merge.allow_full_auto_changed");
    assert_eq!(change.project(), None);
    assert_eq!(change.to_value()["allow_full_auto"], false);
}

fn step(auto: bool, risk: RiskVerdict) -> ResultStep {
    ResultStep { auto, risk }
}

const CLEAR: RiskVerdict = RiskVerdict::Clear { warned: false };

fn stop_of(class: StopClass, rule: &'static str) -> RiskVerdict {
    RiskVerdict::Stop {
        class,
        rule: Some(rule),
    }
}

/// M4 close: the merge result decides an auto-merge entry; the stop that
/// appears first at its step is its own, one already there before it cuts
/// the batch (a person's entry brought it in).
#[test]
fn the_merge_result_stops_the_auto_entry_that_brings_the_stop_in() {
    assert_eq!(merge_result_decision(&[]), MergeResultDecision::Land);
    // People only: never stopped by auto-merge.
    let people = [
        step(false, stop_of(StopClass::DataLoss, "sql_drop")),
        step(false, CLEAR),
    ];
    assert_eq!(merge_result_decision(&people), MergeResultDecision::Land);
    // Two clean ones, risky together: the second one stops.
    let combined = [
        step(true, CLEAR),
        step(
            true,
            stop_of(StopClass::ProgramBreakage, "mass_deletion_files"),
        ),
    ];
    assert_eq!(
        merge_result_decision(&combined),
        MergeResultDecision::Stop {
            index: 1,
            class: StopClass::ProgramBreakage,
            rule: Some("mass_deletion_files"),
        }
    );
    // Alone (a batch of one): its own merge result.
    assert_eq!(
        merge_result_decision(&[step(true, stop_of(StopClass::Leak, "secret_in_content"))]),
        MergeResultDecision::Stop {
            index: 0,
            class: StopClass::Leak,
            rule: Some("secret_in_content"),
        }
    );
    // A person's entry made it a stop: cut before the auto-merge entry.
    let after_person = [
        step(true, CLEAR),
        step(false, stop_of(StopClass::DataLoss, "sql_drop")),
        step(true, stop_of(StopClass::DataLoss, "sql_drop")),
    ];
    assert_eq!(
        merge_result_decision(&after_person),
        MergeResultDecision::Cut { limit: 2 }
    );
    // Too large to assess only combined: cut, judged again smaller;
    // alone it stops.
    let large = [
        step(true, CLEAR),
        step(true, stop_of(StopClass::NotAssessed, "content_not_scanned")),
    ];
    assert_eq!(
        merge_result_decision(&large),
        MergeResultDecision::Cut { limit: 1 }
    );
    assert_eq!(
        merge_result_decision(&[step(
            true,
            stop_of(StopClass::ProgramBreakage, "too_many_changes")
        )]),
        MergeResultDecision::Stop {
            index: 0,
            class: StopClass::ProgramBreakage,
            rule: Some("too_many_changes"),
        }
    );
    for rule in SIZE_LIMITED_RULES {
        assert!(
            RULES.iter().any(|r| r.id == *rule),
            "{rule} is a forge/risk rule"
        );
    }
    // Review LOW: people's entries after the last auto-merge entry make
    // the final tree a stop: cut before the first auto-merge entry.
    let tail = [
        step(false, CLEAR),
        step(true, CLEAR),
        step(false, stop_of(StopClass::Leak, "secret_in_content")),
    ];
    assert_eq!(
        merge_result_decision(&tail),
        MergeResultDecision::Cut { limit: 1 }
    );
    let tail_first = [
        step(true, CLEAR),
        step(false, stop_of(StopClass::Leak, "secret_in_content")),
    ];
    assert_eq!(
        merge_result_decision(&tail_first),
        MergeResultDecision::Cut { limit: 1 }
    );
    // An answer that is not one is never clear.
    assert_eq!(
        merge_result_decision(&[step(true, RiskVerdict::Pending)]),
        MergeResultDecision::Stop {
            index: 0,
            class: StopClass::NotAssessed,
            rule: None,
        }
    );
}

/// The queue's removal of a stopped merge result holds the same head, with
/// the class in the reason word.
#[test]
fn a_merge_result_removal_stops_the_same_head_with_its_class() {
    for class in StopClass::ALL {
        let reason = merge_result_reason(class);
        assert!(reason.len() <= 64, "{reason}");
        assert_eq!(merge_result_class(&reason), Some(class));
        let facts = Facts {
            queue: QueueFacts::Removed { reason },
            ..clear_facts()
        };
        assert_eq!(
            decide(&facts),
            Verdict::Stopped(Stop {
                class,
                reason: "merge_result_risk",
                detail: None,
            })
        );
    }
    assert_eq!(merge_result_class("merge_result_nonsense"), None);
    assert_eq!(merge_result_class(MERGE_RESULT_UNAVAILABLE), None);
    let unavailable = Facts {
        queue: QueueFacts::Removed {
            reason: MERGE_RESULT_UNAVAILABLE.to_owned(),
        },
        ..clear_facts()
    };
    assert_eq!(
        decide(&unavailable),
        Verdict::Stopped(Stop {
            class: StopClass::NotAssessed,
            reason: MERGE_RESULT_UNAVAILABLE,
            detail: None,
        })
    );
    assert_eq!(merge_result_class("conflict"), None);
}

/// Load test 2026-11-17 F5 (ADR-0030 Amendment 2026-11-17): a new
/// project is `full` without required checks, and a document whose
/// `forge/doc-lint` failed was merged by auto-merge. A failed doc-lint on
/// the head now stops it, named like a red required check.
#[test]
fn a_failed_doc_lint_stops_auto_merge_without_being_a_required_check() {
    let facts = Facts {
        doc_lint: DocLintState::of(run(CheckConclusion::Failure)),
        ..clear_facts()
    };
    assert_eq!(
        decide(&facts),
        Verdict::Stopped(Stop {
            class: StopClass::ProgramBreakage,
            reason: "doc_lint_failed",
            detail: Some("forge/doc-lint".to_owned()),
        })
    );
    // A stop outranks every wait.
    let facts = Facts {
        blockers: vec![Blocker {
            reason: "approvals_missing",
            red_check: None,
        }],
        ..facts
    };
    assert!(matches!(
        decide(&facts),
        Verdict::Stopped(Stop {
            reason: "doc_lint_failed",
            ..
        })
    ));
}

/// Not reported on the head yet (or still running): wait for it, so a
/// failing document cannot slip in before its lint.
#[test]
fn auto_merge_waits_for_doc_lint_on_the_head() {
    for state in [
        DocLintState::of(None),
        DocLintState::of(CheckRun::new(CheckStatus::InProgress, None)),
        DocLintState::of(CheckRun::new(CheckStatus::Queued, None)),
    ] {
        assert_eq!(state, DocLintState::Pending);
        let facts = Facts {
            doc_lint: state,
            ..clear_facts()
        };
        assert_eq!(
            decide(&facts),
            Verdict::Waiting {
                reason: "doc_lint_pending"
            }
        );
    }
}

/// A repository without documents gets `success`; a bound hit with no
/// problem is `neutral`. Neither stops auto-merge.
#[test]
fn a_doc_lint_without_a_problem_never_stops_auto_merge() {
    for conclusion in [CheckConclusion::Success, CheckConclusion::Neutral] {
        let facts = Facts {
            doc_lint: DocLintState::of(run(conclusion)),
            ..clear_facts()
        };
        assert_eq!(
            decide(&facts),
            Verdict::Enqueue { warned: false },
            "{conclusion:?}"
        );
    }
}

/// The mode, the emergency stop and the queue still come first: `off`
/// never shows a doc-lint line, a live entry stays queued.
#[test]
fn doc_lint_is_read_only_where_auto_merge_acts() {
    let failed = DocLintState::of(run(CheckConclusion::Failure));
    let off = Facts {
        mode: AutoMergeMode::Off,
        doc_lint: failed,
        ..clear_facts()
    };
    assert_eq!(decide(&off), Verdict::Inactive);
    let queued = Facts {
        queue: QueueFacts::Live { auto: false },
        doc_lint: failed,
        ..clear_facts()
    };
    assert_eq!(decide(&queued), Verdict::Queued { auto: false });
}

/// Review 2026-11-17: a lint run that itself failed (`error`) never opens
/// the gate — it is asked for again (a wait), like a missing run.
#[test]
fn an_errored_doc_lint_waits_like_a_missing_one() {
    for conclusion in [CheckConclusion::Error, CheckConclusion::ActionRequired] {
        let state = DocLintState::of(run(conclusion));
        assert_eq!(state, DocLintState::Errored, "{conclusion:?}");
        assert!(state.needs_run());
        let facts = Facts {
            doc_lint: state,
            ..clear_facts()
        };
        assert_eq!(
            decide(&facts),
            Verdict::Waiting {
                reason: DOC_LINT_PENDING
            }
        );
    }
}

/// Review 2026-11-17: never an endless wait. Past the limit a missing run
/// stops `doc_lint_missing`, an errored one `doc_lint_error` (both
/// `not_assessed`, named); before it, and on a first look, it waits.
#[test]
fn a_doc_lint_still_missing_past_the_limit_stops_with_its_reason() {
    let limit = DOC_LINT_WAIT_LIMIT;
    assert_eq!(limit, std::time::Duration::from_secs(600));
    for (state, reason) in [
        (DocLintState::Pending, DOC_LINT_MISSING),
        (DocLintState::Errored, DOC_LINT_ERROR),
    ] {
        assert_eq!(state.after_waiting(None, limit), state, "a first look");
        assert_eq!(
            state.after_waiting(Some(limit - std::time::Duration::from_secs(1)), limit),
            state
        );
        let overdue = state.after_waiting(Some(limit), limit);
        assert!(!overdue.needs_run(), "no more asking");
        let facts = Facts {
            doc_lint: overdue,
            ..clear_facts()
        };
        assert_eq!(
            decide(&facts),
            Verdict::Stopped(Stop {
                class: StopClass::NotAssessed,
                reason,
                detail: Some("forge/doc-lint".to_owned()),
            })
        );
    }
    // A run that is there is never made overdue.
    for state in [DocLintState::Clear, DocLintState::Failed] {
        assert_eq!(state.after_waiting(Some(limit * 10), limit), state);
    }
}
