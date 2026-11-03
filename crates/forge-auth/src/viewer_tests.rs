//! The viewer flags (U2, `viewer.rs`) against the permission matrix: every
//! row of the actions the flags stand for, whose context is the one the
//! flag's trial builds, gets the flag the row expects — allowed rows
//! `true`, denied rows `false`. A flag that disagreed with `authorize()`
//! would show a control the route then refuses (or hide one it allows).

use forge_core::ProjectId;

use crate::matrix::{Expect, Fixture, Row, ROWS};
use crate::model::{CodeownersToggle, MergeRequestEdit, Resource};
use crate::viewer::TRIAL_SUSPENSION_MINUTES;
use crate::{
    Action, CodeownersToggleCtx, LoadedMergeRequest, LoadedProject, MergeRequestCtx,
    MergeSettingsCtx, ProjectCtx, ProjectView,
};

fn loaded(view: ProjectView) -> LoadedProject {
    LoadedProject {
        id: ProjectId::new(1),
        visibility: view.visibility,
        role: view.role,
        pending_deletion: view.pending_deletion,
    }
}

/// The flag of `row` and the flag's value for the row's actor and
/// fixture; `None` when the row's context is not the one the flag's trial
/// builds (an update that changes the target, another suspension length,
/// enforcing again, …).
fn flag_of(row: &Row) -> Option<(&'static str, bool)> {
    let principal = row.actor.principal();
    match (row.action, row.fixture, row.fixture.resource(row.actor)) {
        (
            action,
            Fixture::Mr(_),
            Resource::MergeRequest {
                project,
                author,
                state,
                draft,
                source_branch,
                target_branch,
                edit,
                target_protection,
            },
        ) => {
            let ctx = MergeRequestCtx::new(
                loaded(project),
                LoadedMergeRequest {
                    iid: Some(1),
                    author,
                    state,
                    draft,
                    source_branch,
                    target_branch,
                },
            );
            let v = ctx.viewer(&principal, target_protection);
            let plain = edit == MergeRequestEdit::default();
            let ready = edit
                == MergeRequestEdit {
                    target_branch: None,
                    draft: Some(false),
                };
            // (action, the row's context is the trial's, flag, value)
            [
                (
                    Action::MergeRequestApprove,
                    plain,
                    "can_approve",
                    v.can_approve,
                ),
                (
                    Action::MergeRequestUnapprove,
                    plain,
                    "can_unapprove",
                    v.can_unapprove,
                ),
                (Action::MergeRequestMerge, plain, "can_merge", v.can_merge),
                (
                    Action::MergeRequestUpdate,
                    ready,
                    "can_mark_ready",
                    v.can_mark_ready,
                ),
            ]
            .into_iter()
            .find(|(a, same, _, _)| *a == action && *same)
            .map(|(_, _, flag, value)| (flag, value))
        }
        (
            Action::MergeSettingsManage,
            _,
            Resource::MergeSettingsChange {
                project,
                highest_protection,
                loosens,
            },
        ) => {
            let p = loaded(project);
            let read = ProjectCtx::new(p).merge_settings_viewer(&principal, highest_protection);
            // The answer of a write builds the flags from its own context.
            let written = MergeSettingsCtx::new(p, highest_protection, loosens).viewer(&principal);
            assert_eq!(read, written, "{row:?}");
            Some(if loosens {
                ("can_loosen", read.can_loosen)
            } else {
                ("can_manage", read.can_manage)
            })
        }
        (
            Action::CodeownersEnforcementToggle,
            _,
            Resource::CodeownersEnforcement {
                project,
                highest_protection,
                change,
            },
        ) => {
            let trial = CodeownersToggle::Suspend {
                minutes: Some(TRIAL_SUSPENSION_MINUTES),
            };
            if change != trial {
                return None;
            }
            let p = loaded(project);
            let read = ProjectCtx::new(p).merge_settings_viewer(&principal, highest_protection);
            let written =
                CodeownersToggleCtx::new(p, highest_protection, change).viewer(&principal);
            assert_eq!(read, written, "{row:?}");
            Some(("can_suspend_codeowners", read.can_suspend_codeowners))
        }
        _ => None,
    }
}

#[test]
fn viewer_flags_equal_authorize_for_every_matrix_row() {
    let mut failures = Vec::new();
    let mut seen: std::collections::BTreeMap<&str, (usize, usize)> = Default::default();
    for row in ROWS.iter() {
        let Some((flag, value)) = flag_of(row) else {
            continue;
        };
        let allowed = row.expect == Expect::Allow;
        let counts = seen.entry(flag).or_default();
        if allowed {
            counts.0 += 1;
        } else {
            counts.1 += 1;
        }
        if value != allowed {
            failures.push(format!(
                "{flag}: {:?} {:?} {:?} expects {:?}, flag {value}",
                row.action, row.actor, row.fixture, row.expect
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // Every flag is held against allowed and denied rows alike.
    for flag in [
        "can_approve",
        "can_unapprove",
        "can_merge",
        "can_mark_ready",
        "can_manage",
        "can_loosen",
        "can_suspend_codeowners",
    ] {
        let (allow, deny) = seen.get(flag).copied().unwrap_or_default();
        assert!(allow > 0 && deny > 0, "{flag}: allow {allow}, deny {deny}");
    }
}
