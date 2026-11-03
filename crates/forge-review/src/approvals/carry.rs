//! Whether a new head may carry the approvals of the old one (M3 R6,
//! ADR-0024 §8.2): [`may_carry_approvals`] is pure, every case fixed by a
//! test. The version job reads its inputs ([`CarryInputs`]) only when the
//! project keeps approvals on push, and forge-db decides again under the
//! merge request's lock (the setting, the target's protection, the head
//! the decision was made for).

use super::codeowners::{self, Codeowners};
use super::{is_config_path, ChangedPathsFact};

/// The target's CODEOWNERS as the carry needs it (owners are not resolved:
/// any rule covering a changed path stops the carry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CarryCodeowners {
    Absent,
    Parsed(Codeowners),
    /// Unreadable, too large, a git failure: the owned paths are unknown.
    Unknown,
}

/// The facts of one carry decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarryInputs {
    /// The project's setting (`true`, the default: new commits clear).
    pub reset_approvals_on_push: bool,
    /// The target has a protection rule.
    pub target_protected: bool,
    /// The paths changed from the old head to the new one
    /// (`diff-tree --no-renames -r`, ADR-0024 §9's reading).
    pub changed: ChangedPathsFact,
    /// The target tip's CODEOWNERS.
    pub codeowners: CarryCodeowners,
}

/// Why the approvals are not carried (the version job's log `reason`).
pub type CarryRefusal = &'static str;

/// ADR-0024 §8.2: the approvals are carried only when the project keeps
/// them (`reset_approvals_on_push = false`) and none of (a) a protected
/// target, (b) a changed configuration path, (c) a changed path a
/// CODEOWNERS rule of the target covers, (d) changed paths that cannot be
/// read in full holds. Every unknown fails closed (no carry). Matching is
/// bounded by [`super::MATCH_BUDGET`] (past it: `codeowners_unknown`); run
/// it on a blocking thread.
pub fn may_carry_approvals(inputs: &CarryInputs) -> Result<(), CarryRefusal> {
    if inputs.reset_approvals_on_push {
        return Err("reset_on_push");
    }
    if inputs.target_protected {
        return Err("target_protected");
    }
    let paths = match &inputs.changed {
        ChangedPathsFact::Known(paths) => paths,
        ChangedPathsFact::Conflict | ChangedPathsFact::Unknown(_) => {
            return Err("changed_paths_unknown")
        }
    };
    if paths.iter().any(|p| is_config_path(p)) {
        return Err("config_path");
    }
    match &inputs.codeowners {
        CarryCodeowners::Absent => Ok(()),
        CarryCodeowners::Unknown => Err("codeowners_unknown"),
        CarryCodeowners::Parsed(file) => match codeowners::owned_lines(
            file,
            paths,
            Some(std::time::Instant::now() + super::MATCH_BUDGET),
        ) {
            Some(lines) if lines.is_empty() => Ok(()),
            Some(_) => Err("owned_path"),
            None => Err("codeowners_unknown"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keep(changed: &[&str]) -> CarryInputs {
        CarryInputs {
            reset_approvals_on_push: false,
            target_protected: false,
            changed: ChangedPathsFact::Known(changed.iter().map(|p| (*p).to_owned()).collect()),
            codeowners: CarryCodeowners::Absent,
        }
    }

    #[test]
    fn approvals_are_carried_only_when_nothing_forbids_it() {
        assert_eq!(may_carry_approvals(&keep(&["src/lib.rs"])), Ok(()));
        assert_eq!(
            may_carry_approvals(&CarryInputs {
                reset_approvals_on_push: true,
                ..keep(&["src/lib.rs"])
            }),
            Err("reset_on_push")
        );
        assert_eq!(
            may_carry_approvals(&CarryInputs {
                target_protected: true,
                ..keep(&["src/lib.rs"])
            }),
            Err("target_protected")
        );
        for config in [".forge/ci.yml", ".forge-ci.yml", "CODEOWNERS", ".Forge/x"] {
            assert_eq!(
                may_carry_approvals(&keep(&["a.txt", config])),
                Err("config_path"),
                "{config}"
            );
        }
        for unknown in [
            ChangedPathsFact::Unknown("too_many_paths"),
            ChangedPathsFact::Unknown("timeout"),
            ChangedPathsFact::Conflict,
        ] {
            assert_eq!(
                may_carry_approvals(&CarryInputs {
                    changed: unknown,
                    ..keep(&[])
                }),
                Err("changed_paths_unknown")
            );
        }
    }

    #[test]
    fn a_path_a_codeowners_rule_covers_stops_the_carry() {
        let file = codeowners::parse(b"/docs/ @alice\n").expect("parses");
        let with = |paths: &[&str]| CarryInputs {
            codeowners: CarryCodeowners::Parsed(file.clone()),
            ..keep(paths)
        };
        assert_eq!(may_carry_approvals(&with(&["src/main.rs"])), Ok(()));
        assert_eq!(
            may_carry_approvals(&with(&["src/main.rs", "docs/guide.md"])),
            Err("owned_path")
        );
        assert_eq!(
            may_carry_approvals(&CarryInputs {
                codeowners: CarryCodeowners::Unknown,
                ..keep(&["src/main.rs"])
            }),
            Err("codeowners_unknown")
        );
    }
}
