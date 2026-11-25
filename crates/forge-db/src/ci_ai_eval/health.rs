//! What `forge doctor` reads about the AI evaluation bench (plan 17 P17-8,
//! §5.3; runbook ai-eval-bench), on the read pool:
//!
//! - [`runner_health`] (`ai_eval.runner`): whether pipelines declare the
//!   report, the `gpu-eval` runners (enabled, live) and the `gpu-eval` jobs
//!   pending past [`NO_RUNNER_AFTER_S`];
//! - [`baseline_health`] (`ai_eval.baseline_age`): per project with an
//!   active schedule whose pipelines declare the report, when the last
//!   scheduled one succeeded (the nightly bench the baseline candidates
//!   come from); the open baseline candidate merge requests (plan 17
//!   P17-6); the projects with `ai_assets.enabled` and no nightly bench.
//!
//! Counts and ids only: no names, no report contents.

use forge_core::ai_eval::BASELINE_BRANCH_PREFIX;
use forge_errors::ForgeError;

use crate::ci_runners::lease::{GPU_EVAL_TAG, NO_RUNNER_AFTER_S, RUNNER_SILENT_AFTER_S};
use crate::{read_query_error, Db};

/// A pipeline that declared the report this recently means the bench is
/// in use.
pub const IN_USE_WINDOW_S: i64 = 7 * 24 * 60 * 60;
/// How far back the nightly bench is looked for.
pub const NIGHTLY_WINDOW_S: i64 = 30 * 24 * 60 * 60;
/// No succeeded nightly bench this long (two nights): the baseline cannot
/// be refreshed (runbook ai-eval-bench: two nights without a bench is
/// SEV3).
pub const NIGHTLY_STALE_AFTER_S: i64 = 48 * 60 * 60;
/// A baseline candidate merge request open this long has missed two weekly
/// review rounds (plan 17 §5.1).
pub const CANDIDATE_STALE_AFTER_S: i64 = 14 * 24 * 60 * 60;
/// The most project ids a list carries.
pub const MAX_IDS: usize = 20;

/// The `gpu-eval` runners and jobs ([`runner_health`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunnerHealth {
    /// Pipelines that declared `eval-report.json` in [`IN_USE_WINDOW_S`].
    pub declaring_pipelines: i64,
    /// Enabled runners tagged `gpu-eval`.
    pub gpu_runners: i64,
    /// Of those, heard from within [`RUNNER_SILENT_AFTER_S`].
    pub live_gpu_runners: i64,
    /// `gpu-eval` jobs pending longer than [`NO_RUNNER_AFTER_S`].
    pub waiting: i64,
    pub oldest_job_id: Option<i64>,
    pub oldest_wait_s: Option<i64>,
}

/// The nightly bench and the baseline candidates ([`baseline_health`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BaselineHealth {
    /// Projects with an active schedule and a scheduled declaring pipeline
    /// in [`NIGHTLY_WINDOW_S`].
    pub nightly_projects: i64,
    /// Of those, the ones whose last succeeded scheduled declaring pipeline
    /// is older than [`NIGHTLY_STALE_AFTER_S`], or absent while their first
    /// one in the window is older than that, by id (at most [`MAX_IDS`]).
    pub stale_projects: Vec<i64>,
    /// How many are stale (the list may be cut).
    pub stale_count: i64,
    /// Over the projects that are not stale, the oldest of their last
    /// successes, in seconds.
    pub fresh_oldest_s: Option<i64>,
    /// Open baseline candidate merge requests.
    pub open_candidates: i64,
    /// The oldest open candidate: (project id, iid, age in seconds).
    pub oldest_candidate: Option<(i64, i64, i64)>,
    /// Projects with `ai_assets.enabled` and no scheduled declaring
    /// pipeline in [`NIGHTLY_WINDOW_S`] (at most [`MAX_IDS`]).
    pub enabled_without_nightly: Vec<i64>,
}

impl BaselineHealth {
    /// The oldest candidate, when it is older than [`CANDIDATE_STALE_AFTER_S`].
    pub fn stale_candidate(&self) -> Option<(i64, i64, i64)> {
        self.oldest_candidate
            .filter(|(_, _, age)| *age > CANDIDATE_STALE_AFTER_S)
    }
}

/// Read [`RunnerHealth`] (the read pool). Errors: `FG-DB-*`.
pub async fn runner_health(db: &Db) -> Result<RunnerHealth, ForgeError> {
    const QUERY: &str = "ci_ai_eval.runner_health";
    let row: (i64, i64, i64, i64, Option<i64>, Option<i64>) = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT \
                   (SELECT count(*) FROM forge.ci_pipelines WHERE ai_eval_job IS NOT NULL \
                     AND created_at > now() - make_interval(secs => $1)), \
                   (SELECT count(*) FROM forge.ci_runners WHERE enabled AND $2 = ANY (tags)), \
                   (SELECT count(*) FROM forge.ci_runners WHERE enabled AND $2 = ANY (tags) \
                     AND last_contact_at > now() - make_interval(secs => $3)), \
                   (SELECT count(*) FROM forge.ci_jobs WHERE status = 'pending' \
                     AND $2 = ANY (tags) AND updated_at < now() - make_interval(secs => $4)), \
                   (SELECT id FROM forge.ci_jobs WHERE status = 'pending' AND $2 = ANY (tags) \
                     AND updated_at < now() - make_interval(secs => $4) \
                     ORDER BY updated_at, id LIMIT 1), \
                   (SELECT floor(extract(epoch FROM now() - min(updated_at)))::bigint \
                     FROM forge.ci_jobs WHERE status = 'pending' AND $2 = ANY (tags) \
                     AND updated_at < now() - make_interval(secs => $4))",
            )
            .bind(IN_USE_WINDOW_S as f64)
            .bind(GPU_EVAL_TAG)
            .bind(RUNNER_SILENT_AFTER_S as f64)
            .bind(NO_RUNNER_AFTER_S as f64)
            .fetch_one(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(RunnerHealth {
        declaring_pipelines: row.0,
        gpu_runners: row.1,
        live_gpu_runners: row.2,
        waiting: row.3,
        oldest_job_id: row.4,
        oldest_wait_s: row.5,
    })
}

/// Read [`BaselineHealth`] (the read pool). Errors: `FG-DB-*`.
pub async fn baseline_health(db: &Db) -> Result<BaselineHealth, ForgeError> {
    const QUERY: &str = "ci_ai_eval.baseline_health";
    let candidate_branch = format!("refs/heads/{BASELINE_BRANCH_PREFIX}%");
    db.within(QUERY, async {
        // Per nightly project: seconds since its last succeeded scheduled
        // declaring pipeline was created (NULL: none in the window), and
        // since its first one in the window was.
        let nightly: Vec<(i64, Option<i64>, i64)> = sqlx::query_as(
            "SELECT p.project_id, \
               floor(extract(epoch FROM now() - max(p.created_at) \
                 FILTER (WHERE p.status = 'succeeded')))::bigint, \
               floor(extract(epoch FROM now() - min(p.created_at)))::bigint \
             FROM forge.ci_pipelines p JOIN forge.projects pr ON pr.id = p.project_id \
             WHERE p.source = 'schedule' AND p.ai_eval_job IS NOT NULL \
               AND p.created_at > now() - make_interval(secs => $1) \
               AND pr.deleted_at IS NULL \
               AND EXISTS (SELECT 1 FROM forge.ci_pipeline_schedules s \
                 WHERE s.project_id = p.project_id AND s.active) \
             GROUP BY p.project_id ORDER BY p.project_id",
        )
        .bind(NIGHTLY_WINDOW_S as f64)
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        let candidates: Vec<(i64, i64, i64, i64)> = sqlx::query_as(
            "SELECT m.project_id, m.iid, \
               floor(extract(epoch FROM now() - m.created_at))::bigint, count(*) OVER () \
             FROM forge.merge_requests m JOIN forge.projects pr ON pr.id = m.project_id \
             WHERE m.state = 'open' AND m.source_branch LIKE $1 AND pr.deleted_at IS NULL \
             ORDER BY m.created_at, m.id LIMIT 1",
        )
        .bind(&candidate_branch)
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        let enabled_without_nightly: Vec<i64> = sqlx::query_scalar(
            "SELECT a.project_id FROM forge.project_ai_assets a \
               JOIN forge.projects pr ON pr.id = a.project_id \
             WHERE a.enabled AND pr.deleted_at IS NULL AND NOT EXISTS ( \
               SELECT 1 FROM forge.ci_pipelines p WHERE p.project_id = a.project_id \
                 AND p.source = 'schedule' AND p.ai_eval_job IS NOT NULL \
                 AND p.created_at > now() - make_interval(secs => $1)) \
             ORDER BY a.project_id LIMIT $2",
        )
        .bind(NIGHTLY_WINDOW_S as f64)
        .bind(MAX_IDS as i64)
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        Ok(summarize(nightly, candidates, enabled_without_nightly))
    })
    .await
}

fn summarize(
    nightly: Vec<(i64, Option<i64>, i64)>,
    candidates: Vec<(i64, i64, i64, i64)>,
    enabled_without_nightly: Vec<i64>,
) -> BaselineHealth {
    let mut health = BaselineHealth {
        nightly_projects: nightly.len() as i64,
        enabled_without_nightly,
        ..BaselineHealth::default()
    };
    for (project, age, first) in nightly {
        match age {
            Some(age) if age <= NIGHTLY_STALE_AFTER_S => {
                let age = age.max(0);
                health.fresh_oldest_s = Some(health.fresh_oldest_s.map_or(age, |o| o.max(age)));
            }
            // A bench first scheduled less than two nights ago has not had
            // its chance yet: neither fresh nor stale.
            None if first <= NIGHTLY_STALE_AFTER_S => {}
            _ => {
                health.stale_count += 1;
                if health.stale_projects.len() < MAX_IDS {
                    health.stale_projects.push(project);
                }
            }
        }
    }
    if let Some((project, iid, age, count)) = candidates.into_iter().next() {
        health.open_candidates = count;
        health.oldest_candidate = Some((project, iid, age.max(0)));
    }
    health
}

#[cfg(test)]
#[path = "health_tests.rs"]
mod tests;
