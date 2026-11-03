//! Whether a new head may carry the approvals of the old one (M3 R6,
//! ADR-0024 §8.2): [`may_carry_approvals`] is pure, every case fixed by a
//! test. The version job reads its inputs ([`CarryInputs`]) only when the
//! project keeps approvals on push, and forge-db decides again under the
//! merge request's lock (the setting, the target's protection, the head
//! the decision was made for).
//!
//! R6 review (HIGH): a carry vouches for code nobody looked at, so it is
//! limited to small, plain edits — never a build / execution surface
//! (`.gitmodules`, `.gitattributes`, a file-mode change or a new
//! executable, a symlink, a submodule), never more than
//! [`CARRY_MAX_FILES`] files or [`CARRY_MAX_LINES`] changed lines, never a
//! binary file. The remaining risk is accepted (ADR-0024 R6): a
//! Maintainer's opt-in, on unprotected targets only.

use forge_git::review_changes::ChangedEntry;

use super::codeowners::{self, Codeowners};
use super::is_config_path;

/// The most changed files a carry accepts.
pub const CARRY_MAX_FILES: usize = 50;
/// The most changed lines (added + deleted) a carry accepts.
pub const CARRY_MAX_LINES: u64 = 1_000;

const MODE_EXEC: u32 = 0o100755;
const MODE_SYMLINK: u32 = 0o120000;
const MODE_GITLINK: u32 = 0o160000;

/// The target's CODEOWNERS as the carry needs it (owners are not resolved:
/// any rule covering a changed path stops the carry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CarryCodeowners {
    Absent,
    Parsed(Codeowners),
    /// Unreadable, too large, a git failure: the owned paths are unknown.
    Unknown,
}

/// The change from the approved head to the new one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CarryChanges {
    /// Every changed entry (`diff-tree --no-renames -r --raw --numstat`).
    Known(Vec<ChangedEntry>),
    /// Not read in full (`too_many_files`, `non_utf8_path`, `timeout`,
    /// `git_failed`, …): never carried.
    Unknown(&'static str),
}

/// The facts of one carry decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarryInputs {
    /// The project's setting (`true`, the default: new commits clear).
    pub reset_approvals_on_push: bool,
    /// The target has a protection rule.
    pub target_protected: bool,
    pub changes: CarryChanges,
    /// The target tip's CODEOWNERS.
    pub codeowners: CarryCodeowners,
}

/// Why the approvals are not carried (the version job's log `reason`).
pub type CarryRefusal = &'static str;

/// The last component of `path`, ASCII case folded.
fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase()
}

/// A build or execution surface (R6 review): never carried.
fn surface(e: &ChangedEntry) -> Option<CarryRefusal> {
    let name = file_name(&e.path);
    if name == ".gitmodules" || name == ".gitattributes" {
        return Some("special_path");
    }
    let modes = [e.old_mode, e.new_mode];
    if modes.contains(&MODE_GITLINK) {
        return Some("submodule");
    }
    if modes.contains(&MODE_SYMLINK) {
        return Some("symlink");
    }
    // A mode change of a kept file, or a new executable.
    if (e.old_mode != 0 && e.new_mode != 0 && e.old_mode != e.new_mode)
        || (e.old_mode == 0 && e.new_mode == MODE_EXEC)
    {
        return Some("mode_change");
    }
    None
}

/// ADR-0024 §8.2 and the R6 review: the approvals are carried only when
/// the project keeps them (`reset_approvals_on_push = false`) and none of
/// (a) a protected target, (b) a changed configuration path, (c) a changed
/// path a CODEOWNERS rule of the target covers, (d) a change that cannot be
/// read in full, (e) a build / execution surface, (f) a binary file, (g)
/// more than [`CARRY_MAX_FILES`] files or [`CARRY_MAX_LINES`] lines holds.
/// Every unknown fails closed (no carry). Matching is bounded by
/// [`super::MATCH_BUDGET`] (past it: `codeowners_unknown`); run it on a
/// blocking thread.
pub fn may_carry_approvals(inputs: &CarryInputs) -> Result<(), CarryRefusal> {
    if inputs.reset_approvals_on_push {
        return Err("reset_on_push");
    }
    if inputs.target_protected {
        return Err("target_protected");
    }
    let entries = match &inputs.changes {
        CarryChanges::Known(entries) => entries,
        CarryChanges::Unknown(_) => return Err("changes_unknown"),
    };
    if entries.len() > CARRY_MAX_FILES {
        return Err("too_many_files");
    }
    if let Some(refusal) = entries.iter().find_map(surface) {
        return Err(refusal);
    }
    let mut lines: u64 = 0;
    for e in entries {
        match (e.added, e.deleted) {
            (Some(a), Some(d)) => lines = lines.saturating_add(a).saturating_add(d),
            _ => return Err("binary_file"),
        }
    }
    if lines > CARRY_MAX_LINES {
        return Err("too_many_lines");
    }
    let paths: Vec<String> = entries.iter().map(|e| e.path.clone()).collect(); // clone: matched as owned paths
    if paths.iter().any(|p| is_config_path(p)) {
        return Err("config_path");
    }
    match &inputs.codeowners {
        CarryCodeowners::Absent => Ok(()),
        CarryCodeowners::Unknown => Err("codeowners_unknown"),
        CarryCodeowners::Parsed(file) => match codeowners::owned_lines(
            file,
            &paths,
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

    fn edit(path: &str, lines: u64) -> ChangedEntry {
        ChangedEntry {
            path: path.to_owned(),
            old_mode: 0o100644,
            new_mode: 0o100644,
            added: Some(lines),
            deleted: Some(0),
        }
    }

    fn keep(entries: Vec<ChangedEntry>) -> CarryInputs {
        CarryInputs {
            reset_approvals_on_push: false,
            target_protected: false,
            changes: CarryChanges::Known(entries),
            codeowners: CarryCodeowners::Absent,
        }
    }

    #[test]
    fn approvals_are_carried_only_when_nothing_forbids_it() {
        assert_eq!(
            may_carry_approvals(&keep(vec![edit("src/lib.rs", 3)])),
            Ok(())
        );
        assert_eq!(
            may_carry_approvals(&CarryInputs {
                reset_approvals_on_push: true,
                ..keep(vec![edit("src/lib.rs", 1)])
            }),
            Err("reset_on_push")
        );
        assert_eq!(
            may_carry_approvals(&CarryInputs {
                target_protected: true,
                ..keep(vec![edit("src/lib.rs", 1)])
            }),
            Err("target_protected")
        );
        for config in [".forge/ci.yml", ".forge-ci.yml", "CODEOWNERS", ".Forge/x"] {
            assert_eq!(
                may_carry_approvals(&keep(vec![edit("a.txt", 1), edit(config, 1)])),
                Err("config_path"),
                "{config}"
            );
        }
        for unknown in ["too_many_files", "timeout", "git_failed", "non_utf8_path"] {
            assert_eq!(
                may_carry_approvals(&CarryInputs {
                    changes: CarryChanges::Unknown(unknown),
                    ..keep(Vec::new())
                }),
                Err("changes_unknown")
            );
        }
    }

    /// R6 review (HIGH): build and execution surfaces are never carried.
    #[test]
    fn a_build_or_execution_surface_is_never_carried() {
        for path in [
            ".gitmodules",
            ".gitattributes",
            "vendor/.gitattributes",
            "x/.GitModules",
        ] {
            assert_eq!(
                may_carry_approvals(&keep(vec![edit(path, 1)])),
                Err("special_path"),
                "{path}"
            );
        }
        let with_modes = |old, new| ChangedEntry {
            old_mode: old,
            new_mode: new,
            ..edit("bin/run", 1)
        };
        // The exec bit set or cleared on a kept file, and a new executable.
        for (old, new) in [(0o100644, 0o100755), (0o100755, 0o100644), (0, 0o100755)] {
            assert_eq!(
                may_carry_approvals(&keep(vec![with_modes(old, new)])),
                Err("mode_change"),
                "{old:o} -> {new:o}"
            );
        }
        // A symlink added, changed or replaced.
        for (old, new) in [(0, 0o120000), (0o120000, 0o120000), (0o100644, 0o120000)] {
            assert_eq!(
                may_carry_approvals(&keep(vec![with_modes(old, new)])),
                Err("symlink")
            );
        }
        // A submodule (gitlink) entry, line counts unknown.
        let gitlink = ChangedEntry {
            added: None,
            deleted: None,
            ..with_modes(0, 0o160000)
        };
        assert_eq!(may_carry_approvals(&keep(vec![gitlink])), Err("submodule"));
        // A plain new file and a deletion are fine.
        assert_eq!(
            may_carry_approvals(&keep(vec![
                with_modes(0, 0o100644),
                with_modes(0o100644, 0)
            ])),
            Ok(())
        );
    }

    /// R6 review (HIGH): the change must stay small and textual.
    #[test]
    fn a_large_or_binary_change_is_never_carried() {
        let files = |n: usize| (0..n).map(|i| edit(&format!("f{i}.txt"), 1)).collect();
        assert_eq!(may_carry_approvals(&keep(files(CARRY_MAX_FILES))), Ok(()));
        assert_eq!(
            may_carry_approvals(&keep(files(CARRY_MAX_FILES + 1))),
            Err("too_many_files")
        );
        assert_eq!(
            may_carry_approvals(&keep(vec![edit("a.txt", CARRY_MAX_LINES)])),
            Ok(())
        );
        assert_eq!(
            may_carry_approvals(&keep(vec![
                edit("a.txt", CARRY_MAX_LINES),
                edit("b.txt", 1)
            ])),
            Err("too_many_lines")
        );
        let binary = ChangedEntry {
            added: None,
            deleted: None,
            ..edit("logo.png", 0)
        };
        assert_eq!(may_carry_approvals(&keep(vec![binary])), Err("binary_file"));
    }

    #[test]
    fn a_path_a_codeowners_rule_covers_stops_the_carry() {
        let file = codeowners::parse(b"/docs/ @alice\n").expect("parses");
        let with = |paths: &[&str]| CarryInputs {
            codeowners: CarryCodeowners::Parsed(file.clone()),
            ..keep(paths.iter().map(|p| edit(p, 1)).collect())
        };
        assert_eq!(may_carry_approvals(&with(&["src/main.rs"])), Ok(()));
        assert_eq!(
            may_carry_approvals(&with(&["src/main.rs", "docs/guide.md"])),
            Err("owned_path")
        );
        assert_eq!(
            may_carry_approvals(&CarryInputs {
                codeowners: CarryCodeowners::Unknown,
                ..keep(vec![edit("src/main.rs", 1)])
            }),
            Err("codeowners_unknown")
        );
    }
}
