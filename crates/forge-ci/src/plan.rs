//! The execution plan (plan 04 P-2): which jobs a trigger runs and in what
//! order, without running anything. Deterministic for the same definition
//! and trigger.
//!
//! A job waits for every selected job of the previous stage that has any
//! (the stage barrier; earlier stages follow transitively) and for its
//! `needs` (which may be in its own stage). Its `wave` is 0 without
//! prerequisites, else one more than its latest prerequisite's; jobs of one
//! wave may run in parallel. The plan lists the jobs by (wave, stage, name).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::definition::{Definition, Job};
use crate::Problem;

/// What started the pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A push to a branch: jobs whose `rules.branches` match it.
    Push,
    /// A merge request's pipeline (on the merge result): jobs with
    /// `rules.merge_requests: true`.
    MergeRequest,
}

impl Event {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Push => "push",
            Self::MergeRequest => "merge_request",
        }
    }

    /// `push` or `merge_request`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "push" => Some(Self::Push),
            "merge_request" => Some(Self::MergeRequest),
            _ => None,
        }
    }
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The event, the branch (for a merge request: its source branch) and
/// whether that branch is protected (the caller decides it from the
/// repository's settings; a merge request's pipeline is never protected).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    pub event: Event,
    pub branch: String,
    pub protected: bool,
}

impl Trigger {
    /// Whether this pipeline may hand secrets to a job: only a push to a
    /// protected branch.
    pub fn may_use_secrets(&self) -> bool {
        self.protected && self.event == Event::Push
    }
}

/// The jobs to run, in execution order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub trigger: Trigger,
    pub jobs: Vec<PlannedJob>,
}

/// One job of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedJob {
    pub name: String,
    pub stage: String,
    pub wave: usize,
    /// The jobs it waits for directly (sorted).
    pub after: Vec<String>,
    pub image: String,
    pub timeout_secs: u32,
    pub tags: Vec<String>,
    pub secrets: Vec<String>,
    pub script_lines: usize,
    /// The names of its services, in file order.
    pub services: Vec<String>,
}

fn selected(job: &Job, trigger: &Trigger) -> bool {
    match trigger.event {
        Event::Push => job.rules.matches_branch(&trigger.branch),
        Event::MergeRequest => job.rules.merge_requests,
    }
}

/// The plan for `trigger`, or the problems: `needs_not_in_pipeline` for a
/// selected job that needs a job the trigger does not run, and
/// `secret_on_unprotected_ref` for a selected job with secrets when the
/// trigger may not use secrets (a merge request, or an unprotected branch).
/// The plan fails rather than drop the secrets silently.
pub fn plan(def: &Definition, trigger: &Trigger) -> Result<Plan, Vec<Problem>> {
    let chosen: BTreeMap<&str, &Job> = def
        .jobs
        .iter()
        .filter(|j| selected(j, trigger))
        .map(|j| (j.name.as_str(), j))
        .collect();
    let mut problems = Vec::new();
    for job in chosen.values() {
        if !job.secrets.is_empty() && !trigger.may_use_secrets() {
            problems.push(Problem::about(
                job.line,
                "secret_on_unprotected_ref",
                job.name.as_str(),
            ));
        }
        for need in &job.needs {
            if !chosen.contains_key(need.as_str()) {
                problems.push(Problem::about(
                    job.line,
                    "needs_not_in_pipeline",
                    need.as_str(),
                ));
            }
        }
    }
    if !problems.is_empty() {
        problems.sort();
        return Err(problems);
    }
    // Selected jobs per stage index.
    let mut by_stage: BTreeMap<usize, Vec<&str>> = BTreeMap::new();
    for job in chosen.values() {
        by_stage
            .entry(job.stage_index)
            .or_default()
            .push(job.name.as_str());
    }
    let after: BTreeMap<&str, BTreeSet<&str>> = chosen
        .values()
        .map(|job| {
            let mut pre: BTreeSet<&str> = job.needs.iter().map(String::as_str).collect();
            if let Some((_, prev)) = by_stage.range(..job.stage_index).next_back() {
                pre.extend(prev.iter().copied());
            }
            (job.name.as_str(), pre)
        })
        .collect();
    // Waves by repeated relaxation in topological order (a valid
    // definition is acyclic: needs never point to a later stage).
    let mut wave: BTreeMap<&str, usize> = BTreeMap::new();
    while wave.len() < after.len() {
        let before = wave.len();
        for (name, pre) in &after {
            if wave.contains_key(name) {
                continue;
            }
            if pre.iter().all(|p| wave.contains_key(p)) {
                let w = pre
                    .iter()
                    .filter_map(|p| wave.get(p))
                    .max()
                    .map_or(0, |m| m + 1);
                wave.insert(name, w);
            }
        }
        if wave.len() == before {
            // Unreachable for a checked definition; refuse rather than loop.
            let stuck = after
                .keys()
                .find(|n| !wave.contains_key(*n))
                .and_then(|n| chosen.get(n));
            let line = stuck.map_or(0, |j| j.line);
            return Err(vec![Problem::new(line, "needs_cycle")]);
        }
    }
    let mut jobs: Vec<PlannedJob> = chosen
        .values()
        // clone: the plan owns its values, independent of the definition
        .map(|job| PlannedJob {
            name: job.name.clone(),
            stage: job.stage.clone(),
            wave: wave.get(job.name.as_str()).copied().unwrap_or(0),
            after: after
                .get(job.name.as_str())
                .map(|s| s.iter().map(|n| (*n).to_owned()).collect())
                .unwrap_or_default(),
            image: job.image.clone(),
            timeout_secs: job.timeout_secs,
            tags: job.tags.clone(),
            secrets: job.secrets.clone(),
            script_lines: job.script.len(),
            services: job.services.iter().map(|s| s.name.clone()).collect(),
        })
        .collect();
    let stage_of: BTreeMap<&str, usize> = chosen
        .values()
        .map(|j| (j.name.as_str(), j.stage_index))
        .collect();
    jobs.sort_by(|a, b| {
        let key = |j: &PlannedJob| (j.wave, stage_of.get(j.name.as_str()).copied().unwrap_or(0));
        key(a).cmp(&key(b)).then_with(|| a.name.cmp(&b.name))
    });
    Ok(Plan {
        // clone: the plan records the trigger it was made for
        trigger: trigger.clone(),
        jobs,
    })
}
