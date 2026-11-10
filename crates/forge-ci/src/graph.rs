//! The checks across jobs: `needs` (unknown, duplicate, later stage, cycle)
//! and secrets on refs that may be unprotected.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::definition::{pattern_matches, Definition, Job};
use crate::{LintContext, Problem};

pub(crate) fn lint(def: &Definition, ctx: &LintContext, problems: &mut Vec<Problem>) {
    let by_name: HashMap<&str, &Job> = def.jobs.iter().map(|j| (j.name.as_str(), j)).collect();
    for job in &def.jobs {
        let mut seen = BTreeSet::new();
        for need in &job.needs {
            if !seen.insert(need.as_str()) {
                problems.push(Problem::about(job.line, "needs_duplicate", need.as_str()));
                continue;
            }
            match by_name.get(need.as_str()) {
                None => problems.push(Problem::about(job.line, "needs_unknown", need.as_str())),
                Some(target) if target.stage_index > job.stage_index => {
                    problems.push(Problem::about(job.line, "needs_later_stage", need.as_str()));
                }
                Some(_) => {}
            }
        }
        lint_secrets(job, ctx, problems);
    }
    for name in cyclic(def) {
        if let Some(job) = by_name.get(name) {
            problems.push(Problem::about(job.line, "needs_cycle", name));
        }
    }
}

/// The jobs on a `needs` cycle (or between two): what is left after peeling
/// every job without remaining needs, then every job nothing remaining
/// needs. Sorted by name; empty for a DAG.
pub(crate) fn cyclic(def: &Definition) -> Vec<&str> {
    // name → its known needs (unknown ones are reported elsewhere).
    let names: BTreeSet<&str> = def.jobs.iter().map(|j| j.name.as_str()).collect();
    let mut needs: BTreeMap<&str, BTreeSet<&str>> = def
        .jobs
        .iter()
        .map(|j| {
            let known = j
                .needs
                .iter()
                .map(String::as_str)
                .filter(|n| names.contains(n))
                .collect();
            (j.name.as_str(), known)
        })
        .collect();
    // Forward: drop jobs whose needs are all gone.
    loop {
        let free: Vec<&str> = needs
            .iter()
            .filter(|(_, ns)| ns.iter().all(|n| !needs.contains_key(n)))
            .map(|(name, _)| *name)
            .collect();
        if free.is_empty() {
            break;
        }
        for name in free {
            needs.remove(name);
        }
    }
    // Backward: drop jobs no remaining job needs.
    loop {
        let needed: BTreeSet<&str> = needs.values().flatten().copied().collect();
        let free: Vec<&str> = needs
            .keys()
            .filter(|name| !needed.contains(*name))
            .copied()
            .collect();
        if free.is_empty() {
            break;
        }
        for name in free {
            needs.remove(name);
        }
    }
    needs.into_keys().collect()
}

/// A job with secrets must not run in a merge request's pipeline (its
/// source branch is not protected) and every branch pattern must be
/// protected: covered by the repository's protected patterns. Without the
/// list ([`LintContext::local_check`]) none is: `secret_protection_unknown`.
fn lint_secrets(job: &Job, ctx: &LintContext, problems: &mut Vec<Problem>) {
    if job.secrets.is_empty() {
        return;
    }
    if job.rules.merge_requests {
        problems.push(Problem::about(
            job.line,
            "secret_on_unprotected_ref",
            "merge_requests",
        ));
    }
    for pattern in &job.rules.branches {
        let list = match ctx.protected_branches() {
            None => {
                problems.push(Problem::about(
                    job.line,
                    "secret_protection_unknown",
                    pattern.as_str(),
                ));
                continue;
            }
            Some(list) => list,
        };
        let protected = list
            .iter()
            .any(|p| p == pattern || (!pattern.ends_with('*') && pattern_matches(p, pattern)));
        if !protected {
            problems.push(Problem::about(
                job.line,
                "secret_on_unprotected_ref",
                pattern.as_str(),
            ));
        }
    }
}
