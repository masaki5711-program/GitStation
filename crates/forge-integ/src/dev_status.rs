//! The development status of an integration's work items (M3 I1d; plan 02
//! §5.2, §8.2, §11.2; ADR-0016 Amendment I1d; docs/api/dev-status.md):
//!
//! - [`Feed::work_item_dev_status`] (`GET
//!   /api/v1/integrations/{id}/work-items/{ref}/dev-status`): one reference
//!   on every external project the visible bindings have (or the one
//!   `project_id` names); a reference of no bound, visible project is
//!   `FG-REPO-0005 resource=work_item reason=unbound_ref`.
//! - [`Feed::dev_status_page`] (`GET /api/v1/integrations/{id}/dev-status`):
//!   every work item of the visible bindings in id order, page by page (the
//!   resync after a 410). The page cursor ([`DevStatusPosition`]) carries
//!   the feed position of the **first** page; a later page checks it like
//!   the feed checks a cursor (`FG-INT-0007` `epoch_mismatch` /
//!   `ahead_of_head` / `retention` / `visible_set_changed`, `FG-INT-0008`
//!   for one that is not this integration's).
//!
//! Both read in one snapshot with the feed's decision (`prove_visible`,
//! the same principal rules) and answer `as_of_seq` with its `feed_cursor`:
//! every event up to `as_of_seq` is reflected (the bound
//! `min(high_seq, work-item-sync)`), so reading the feed from
//! `feed_cursor` misses nothing (events after it may already be reflected:
//! at least once, like the feed). Both share the feed's one-read-at-a-time
//! slot per integration.
//!
//! Each merge request names its author and who merged it; their employee
//! numbers are added (one read on the write pool, like the feed's actor)
//! only for a token holding `user_employee_number`.

use std::collections::BTreeMap;
use std::time::Duration;

use forge_auth::{Action, Authorized, IntegrationFeedCtx};
use forge_core::{DevStatusPosition, IntegrationId, WorkItemRef};
use forge_db::feed::{
    self as db_feed, DevCheckRun, DevItemStatus, DevWorkItem, MrStateFilter, MAX_DEV_STATUS_LIMIT,
    MAX_PAGE_MERGE_REQUESTS,
};
use forge_errors::{ErrorCode, ForgeError};

use crate::{internal, personal_scopes, Decided, Feed};

pub use forge_core::checks::{CheckConclusion, CheckStatus};
pub use forge_db::feed::{DevMergeRequest, DevPerson, DEFAULT_DEV_STATUS_LIMIT};

/// What `GET …/work-items/{ref}/dev-status` asks for (parsed by the route).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItemQuery {
    pub reference: WorkItemRef,
    /// The external (kanflow) project id; `None`: every bound one.
    pub project_id: Option<String>,
    pub state: MrStateFilter,
}

/// What `GET …/dev-status` asks for (parsed and bounded by the route).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageQuery {
    pub cursor: Option<String>,
    /// 1 ..= [`MAX_DEV_STATUS_LIMIT`].
    pub limit: usize,
    pub state: MrStateFilter,
}

/// The development status of one work item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItemStatus {
    pub provider: String,
    pub project_id: String,
    pub reference: String,
    /// Newest first.
    pub merge_requests: Vec<DevMergeRequest>,
    /// More merge requests are linked than listed.
    pub truncated: bool,
}

/// One answer (one work item, or one page).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevStatus {
    /// Every event with `seq <= as_of_seq` is reflected.
    pub as_of_seq: i64,
    /// The feed's cursor at `as_of_seq` (`GET …/events?cursor=`).
    pub feed_cursor: String,
    pub work_items: Vec<WorkItemStatus>,
    /// The next page (`None`: this was the last; one work item's answer
    /// never has one).
    pub next_cursor: Option<String>,
    pub has_more: bool,
}

/// The checks of a head in one word: `none` (no run reported — never
/// success), `pending` (a run not completed), `failure` (a completed run
/// concluded `failure`, `error` or `action_required`), `neutral` (every run
/// completed, some `neutral`), `success` (every run `success`). The merge
/// status (`GET …/status`) decides what a merge needs; this is a summary.
pub fn checks_state(runs: &[DevCheckRun]) -> &'static str {
    if runs.is_empty() {
        return "none";
    }
    let failed = runs.iter().any(|r| {
        matches!(
            r.conclusion,
            Some(
                CheckConclusion::Failure | CheckConclusion::Error | CheckConclusion::ActionRequired
            )
        )
    });
    if failed {
        return "failure";
    }
    if runs.iter().any(|r| r.status != CheckStatus::Completed) {
        return "pending";
    }
    if runs
        .iter()
        .all(|r| r.conclusion == Some(CheckConclusion::Success))
    {
        "success"
    } else {
        "neutral"
    }
}

/// `FG-REPO-0005`: no bound, visible project has that reference's
/// external project (plan 11 `unbound_ref_is_404`).
fn unbound(integration: IntegrationId) -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", "work_item")
        .with_context("reason", "unbound_ref")
        .with_context("integration_id", integration.get())
}

fn status_of(item: DevWorkItem, status: Option<DevItemStatus>) -> WorkItemStatus {
    let status = status.unwrap_or_default();
    WorkItemStatus {
        provider: item.provider,
        project_id: item.external_project_id,
        reference: item.reference,
        merge_requests: status.merge_requests,
        truncated: status.truncated,
    }
}

impl Feed {
    /// Add the employee numbers of the merge requests' people for a token
    /// holding `user_employee_number` (read on the write pool, all of them
    /// at once; the read pool never sees them). Without the scope nothing
    /// is read and nothing is added.
    async fn with_employee_numbers(
        &self,
        proof: &Authorized<IntegrationFeedCtx>,
        statuses: &mut BTreeMap<i64, DevItemStatus>,
    ) -> Result<(), ForgeError> {
        let (_, employee) = personal_scopes(proof);
        if !employee {
            return Ok(());
        }
        let people = || {
            statuses
                .values()
                .flat_map(|s| &s.merge_requests)
                .flat_map(|mr| [Some(&mr.author), mr.merged_by.as_ref()])
                .flatten()
        };
        let mut users: Vec<_> = people().map(|p| p.id).collect();
        users.sort();
        users.dedup();
        let facts = db_feed::personal_facts(&self.db, &users).await?;
        for status in statuses.values_mut() {
            for mr in &mut status.merge_requests {
                for person in [Some(&mut mr.author), mr.merged_by.as_mut()]
                    .into_iter()
                    .flatten()
                {
                    if let Some((_, number)) = facts.get(&person.id) {
                        person.employee_number.clone_from(number);
                    }
                }
            }
        }
        Ok(())
    }

    /// The feed position of a page cursor, checked like a feed cursor.
    fn page_start(
        &self,
        d: &Decided,
        integration: IntegrationId,
        cursor: &str,
    ) -> Result<(i64, i64), ForgeError> {
        let days = u64::try_from(d.state.retention_days.max(1)).unwrap_or(1);
        let max_age = Duration::from_secs(days * 86_400 * 3 / 2);
        let at = DevStatusPosition::open(&self.keys, integration, cursor, max_age)?;
        let seq = self.check_position(d, integration, &at.feed)?;
        Ok((seq, at.after_work_item))
    }

    /// `GET /api/v1/integrations/{id}/work-items/{ref}/dev-status`.
    pub async fn work_item_dev_status(
        &self,
        proof: &Authorized<IntegrationFeedCtx>,
        query: &WorkItemQuery,
    ) -> Result<DevStatus, ForgeError> {
        if proof.action() != Action::IntegrationDevStatusRead {
            return Err(internal("proof_action_mismatch"));
        }
        let integration = *proof.id();
        let _slot = self.in_flight.enter(integration)?;
        let mut d = self.decide(proof).await?;
        let answer = async {
            let items = d
                .snapshot
                .bound_work_items(&d.visible, &query.reference, query.project_id.as_deref())
                .await?;
            if items.is_empty() {
                return Err(unbound(integration));
            }
            let ids: Vec<i64> = items.iter().filter_map(|i| i.id).collect();
            let mut statuses = d
                .snapshot
                .dev_status_of(&d.visible, &ids, query.state, None)
                .await?
                .statuses;
            self.with_employee_numbers(proof, &mut statuses).await?;
            let as_of = d.state.bound();
            let feed_cursor = self.position(&d, integration, as_of).seal(&self.keys)?;
            let work_items = items
                .into_iter()
                .map(|item| {
                    let status = item.id.and_then(|id| statuses.remove(&id));
                    status_of(item, status)
                })
                .collect();
            Ok(DevStatus {
                as_of_seq: as_of,
                feed_cursor,
                work_items,
                next_cursor: None,
                has_more: false,
            })
        }
        .await;
        d.snapshot.close().await;
        answer
    }

    /// `GET /api/v1/integrations/{id}/dev-status?cursor=…&limit=…`: one
    /// page of every work item (without a cursor: the first, as of now).
    pub async fn dev_status_page(
        &self,
        proof: &Authorized<IntegrationFeedCtx>,
        query: &PageQuery,
    ) -> Result<DevStatus, ForgeError> {
        if proof.action() != Action::IntegrationDevStatusRead {
            return Err(internal("proof_action_mismatch"));
        }
        if query.limit == 0 || query.limit > MAX_DEV_STATUS_LIMIT {
            return Err(internal("dev_status_limit_out_of_range"));
        }
        let integration = *proof.id();
        let _slot = self.in_flight.enter(integration)?;
        let mut d = self.decide(proof).await?;
        let answer = async {
            let (as_of, after) = match query.cursor.as_deref() {
                Some(cursor) => self.page_start(&d, integration, cursor)?,
                None => (d.state.bound(), 0),
            };
            let mut items = d
                .snapshot
                .work_items_after(&d.visible, after, query.limit)
                .await?;
            let mut has_more = items.len() > query.limit;
            items.truncate(query.limit);
            let ids: Vec<i64> = items.iter().filter_map(|i| i.id).collect();
            // At most MAX_PAGE_MERGE_REQUESTS merge requests (at least one
            // work item), cut inside the read before any check run is
            // fetched: the rest is the next page's.
            let read = d
                .snapshot
                .dev_status_of(&d.visible, &ids, query.state, Some(MAX_PAGE_MERGE_REQUESTS))
                .await?;
            let mut statuses = read.statuses;
            self.with_employee_numbers(proof, &mut statuses).await?;
            if let Some(last) = read.cut_after {
                items.retain(|i| i.id.is_some_and(|id| id <= last));
                has_more = true;
            }
            let start = self.position(&d, integration, as_of);
            let feed_cursor = start.seal(&self.keys)?;
            let next_cursor = match (has_more, items.last().and_then(|i| i.id)) {
                (true, Some(last)) => Some(
                    DevStatusPosition {
                        feed: start,
                        after_work_item: last,
                    }
                    .seal(&self.keys)?,
                ),
                _ => None,
            };
            let work_items = items
                .into_iter()
                .map(|item| {
                    let status = item.id.and_then(|id| statuses.remove(&id));
                    status_of(item, status)
                })
                .collect();
            Ok(DevStatus {
                as_of_seq: as_of,
                feed_cursor,
                work_items,
                has_more: next_cursor.is_some(),
                next_cursor,
            })
        }
        .await;
        d.snapshot.close().await;
        answer
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    fn run(status: CheckStatus, conclusion: Option<CheckConclusion>) -> DevCheckRun {
        DevCheckRun {
            name: "ci/gates".to_owned(),
            status,
            conclusion,
        }
    }

    /// Plan 11 I1 `no_checks_is_distinct_from_success`: no run is `none`,
    /// never `success`; a failure wins over a run in progress.
    #[test]
    fn the_checks_summary_never_takes_no_run_for_success() {
        use CheckConclusion as C;
        use CheckStatus as S;
        assert_eq!(checks_state(&[]), "none");
        assert_eq!(
            checks_state(&[run(S::Completed, Some(C::Success))]),
            "success"
        );
        assert_eq!(
            checks_state(&[
                run(S::Completed, Some(C::Success)),
                run(S::Completed, Some(C::Neutral))
            ]),
            "neutral"
        );
        assert_eq!(
            checks_state(&[
                run(S::Completed, Some(C::Success)),
                run(S::InProgress, None)
            ]),
            "pending"
        );
        assert_eq!(
            checks_state(&[
                run(S::Queued, None),
                run(S::Completed, Some(C::ActionRequired))
            ]),
            "failure"
        );
        assert_eq!(
            checks_state(&[run(S::Completed, Some(C::Error))]),
            "failure"
        );
    }
}
