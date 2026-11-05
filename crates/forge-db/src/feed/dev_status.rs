//! The development status of an integration's work items (M3 I1d; plan 02
//! §5.2, §8.2, §11.2; ADR-0016 Amendment I1d), read in the feed's snapshot
//! ([`FeedSnapshot`]) with the feed's decision ([`VisibleSet`]):
//!
//! - [`FeedSnapshot::bound_work_items`] / [`FeedSnapshot::work_items_after`]:
//!   the work items of the integration whose external (kanflow) project is
//!   bound to a visible project — one reference (each external project
//!   once, with its row if it exists) or every work item in id order.
//! - [`FeedSnapshot::dev_status_of`]: for those work items, the merge
//!   requests with an **open** link (`merge_request_work_items.unlinked_at
//!   IS NULL`) in a visible project whose binding is the work item's — open,
//!   merged and closed alike (02 §8.2: a closed merge request is still
//!   linked) — with the approvals given on the head and target, the head's
//!   check runs (M3 C1), and the time it merged (the applied attempt).
//!   The integration's allowlist narrows them like the feed: approvals only
//!   with `merge_request.approved` / `.unapproved` admitted, check runs
//!   only with `check.completed` (I1d review).
//!
//! Only the set's project ids turn into SQL; nothing here decides. The
//! caller (forge-integ) proved the set with `forge_auth::prove_visible`.

use std::collections::BTreeMap;

use forge_core::checks::{CheckConclusion, CheckStatus};
use forge_core::{ProjectId, VisibleSet, WorkItemRef};
use forge_errors::{ErrorCode, ForgeError};

use super::FeedSnapshot;
use crate::read_query_error;

/// The most merge requests listed for one work item (`truncated` beyond;
/// newest first).
pub const MAX_MERGE_REQUESTS_PER_ITEM: usize = 100;
/// The most work items of one development-status page.
pub const MAX_DEV_STATUS_LIMIT: usize = 100;
/// `limit` of a page when the request names none.
pub const DEFAULT_DEV_STATUS_LIMIT: usize = 50;
/// A page stops adding work items once it holds this many merge requests
/// (at least one work item is always returned): bounds the answer to a few
/// MiB whatever the links.
pub const MAX_PAGE_MERGE_REQUESTS: usize = 1000;

const TS: &str = "'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'";

/// Which merge requests a status lists (`state`, plan 02 §5.2: `all` by
/// default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MrStateFilter {
    #[default]
    All,
    Open,
    Merged,
    Closed,
}

impl MrStateFilter {
    pub const ALL: [MrStateFilter; 4] = [Self::All, Self::Open, Self::Merged, Self::Closed];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Open => "open",
            Self::Merged => "merged",
            Self::Closed => "closed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == raw)
    }

    /// The `merge_requests.state` to match (`None`: every state).
    fn state(self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::Open => Some("open"),
            Self::Merged => Some("merged"),
            Self::Closed => Some("closed"),
        }
    }
}

/// One work item as the status names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevWorkItem {
    /// `None`: no row yet — nothing ever named it (its merge requests are
    /// then none).
    pub id: Option<i64>,
    /// The integration's kind (`kanflow`).
    pub provider: String,
    /// The external (kanflow) project id of the binding.
    pub external_project_id: String,
    /// `PBI-<n>`.
    pub reference: String,
}

/// One check run on a merge request's head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevCheckRun {
    pub name: String,
    pub status: CheckStatus,
    pub conclusion: Option<CheckConclusion>,
}

/// One linked merge request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevMergeRequest {
    pub project_id: ProjectId,
    /// The project's path now (`None`: no namespace row, never expected).
    pub project_path: Option<String>,
    pub iid: i64,
    pub title: String,
    /// `open`, `merged` or `closed`.
    pub state: String,
    pub draft: bool,
    /// Without `refs/heads/`.
    pub source_branch: String,
    pub target_branch: String,
    pub head_sha: Option<String>,
    /// UTC, RFC 3339 (seconds).
    pub created_at: String,
    pub updated_at: String,
    /// When the applied merge attempt finished (`None`: not merged, or no
    /// attempt recorded).
    pub merged_at: Option<String>,
    pub closed_at: Option<String>,
    /// Live approvals on the current head and target (counted or not: the
    /// merge status, `GET …/status`, decides what counts). `None`: the
    /// integration's allowlist admits no approval event (I1d review).
    pub approvals_given: Option<u32>,
    /// Every run on the head (empty: none reported, or no head). `None`: the
    /// allowlist does not admit `check.completed` (I1d review).
    pub checks: Option<Vec<DevCheckRun>>,
    /// How the open link was made (`auto` / `manual`).
    pub link_source: String,
    pub linked_at: String,
}

/// What [`FeedSnapshot::dev_status_of`] read: the merge requests by work
/// item, and — when a page budget cut the work items — the last one kept.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DevStatusRead {
    pub statuses: BTreeMap<i64, DevItemStatus>,
    /// `Some(id)`: the work items after `id` did not fit the budget (the
    /// caller drops them; they start the next page).
    pub cut_after: Option<i64>,
}

/// The merge requests of one work item.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DevItemStatus {
    /// Newest first, at most [`MAX_MERGE_REQUESTS_PER_ITEM`].
    pub merge_requests: Vec<DevMergeRequest>,
    /// More were linked than listed.
    pub truncated: bool,
}

fn branch(raw: String) -> String {
    raw.strip_prefix("refs/heads/")
        .map(str::to_owned)
        .unwrap_or(raw)
}

fn invalid(reason: &'static str, id: i64) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("merge_request_id", id)
}

type MrRow = (
    i64,
    i64,
    i64,
    Option<String>,
    i64,
    String,
    String,
    bool,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    (String, String, String, String),
);

impl FeedSnapshot {
    /// The external projects of the visible bindings (sorted, unique),
    /// each with the integration's kind.
    fn visible_externals_sql() -> &'static str {
        "SELECT DISTINCT b.external_project_id, i.kind FROM forge.integration_bindings b \
         JOIN forge.integrations i ON i.id = b.integration_id \
         WHERE b.integration_id = $1 AND b.project_id = ANY($2) \
         ORDER BY b.external_project_id"
    }

    /// The work items named `reference` on the external projects the visible
    /// bindings have — `external`, when given, only that one. One entry per
    /// external project (empty: none of them is bound and visible — the
    /// route's `FG-REPO-0005`).
    pub async fn bound_work_items(
        &mut self,
        visible: &VisibleSet,
        reference: &WorkItemRef,
        external: Option<&str>,
    ) -> Result<Vec<DevWorkItem>, ForgeError> {
        const QUERY: &str = "feed.dev_status.work_item";
        if visible.projects().is_empty() {
            return Ok(Vec::new());
        }
        let integration = visible.integration().get();
        let projects = visible.project_ids();
        let db = self.db.clone();
        let conn = self.conn();
        db.within(QUERY, async {
            let externals: Vec<(String, String)> = sqlx::query_as(Self::visible_externals_sql())
                .bind(integration)
                .bind(&projects)
                .fetch_all(&mut *conn)
                .await
                .map_err(|e| read_query_error(QUERY, e))?;
            let externals: Vec<(String, String)> = externals
                .into_iter()
                .filter(|(e, _)| external.is_none_or(|wanted| wanted == e))
                .collect();
            if externals.is_empty() {
                return Ok(Vec::new());
            }
            let names: Vec<&str> = externals.iter().map(|(e, _)| e.as_str()).collect();
            let rows: Vec<(i64, String)> = sqlx::query_as(
                "SELECT w.id, w.external_project_id FROM forge.work_items w \
                 WHERE w.integration_id = $1 AND w.ref = $2 AND w.external_project_id = ANY($3)",
            )
            .bind(integration)
            .bind(reference.as_str())
            .bind(&names)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
            Ok(externals
                .into_iter()
                .map(|(ext, kind)| DevWorkItem {
                    id: rows.iter().find(|r| r.1 == ext).map(|r| r.0),
                    provider: kind,
                    external_project_id: ext,
                    reference: reference.as_str().to_owned(),
                })
                .collect())
        })
        .await
    }

    /// Up to `limit + 1` work items of the integration after `after` (by
    /// id) on the visible bindings' external projects — the paged status
    /// (the extra one says whether more remain).
    /// I1d review: a work item whose only open links are in projects the
    /// set does not show is not walked (it would name a reference only a
    /// hidden project used); one with a visible link, or with no open link
    /// at all, is.
    pub async fn work_items_after(
        &mut self,
        visible: &VisibleSet,
        after: i64,
        limit: usize,
    ) -> Result<Vec<DevWorkItem>, ForgeError> {
        const QUERY: &str = "feed.dev_status.work_items";
        if limit == 0 || limit > MAX_DEV_STATUS_LIMIT {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "dev_status_limit_out_of_range")
                .with_context("limit", limit));
        }
        if visible.projects().is_empty() {
            return Ok(Vec::new());
        }
        let integration = visible.integration().get();
        let projects = visible.project_ids();
        let wanted = i64::try_from(limit).unwrap_or(i64::MAX).saturating_add(1);
        let db = self.db.clone();
        let conn = self.conn();
        db.within(QUERY, async {
            let rows: Vec<(i64, String, String, String)> = sqlx::query_as(
                "SELECT w.id, i.kind, w.external_project_id, w.ref FROM forge.work_items w \
                 JOIN forge.integrations i ON i.id = w.integration_id \
                 WHERE w.integration_id = $1 AND w.id > $3 \
                   AND w.external_project_id IN (SELECT b.external_project_id \
                     FROM forge.integration_bindings b \
                     WHERE b.integration_id = $1 AND b.project_id = ANY($2)) \
                   AND (EXISTS (SELECT 1 FROM forge.merge_request_work_items l \
                         WHERE l.work_item_id = w.id AND l.unlinked_at IS NULL \
                           AND l.project_id = ANY($2)) \
                     OR NOT EXISTS (SELECT 1 FROM forge.merge_request_work_items l \
                         WHERE l.work_item_id = w.id AND l.unlinked_at IS NULL)) \
                 ORDER BY w.id LIMIT $4",
            )
            .bind(integration)
            .bind(&projects)
            .bind(after)
            .bind(wanted)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
            Ok(rows
                .into_iter()
                .map(|(id, kind, ext, reference)| DevWorkItem {
                    id: Some(id),
                    provider: kind,
                    external_project_id: ext,
                    reference,
                })
                .collect())
        })
        .await
    }

    /// The linked merge requests of `items` (work item ids, ascending) that
    /// `visible` shows, `state` narrowing them, by work item. A work item
    /// without an open link has no entry. Nothing when the set shows no
    /// merge request (no visible project, or an allowlist without
    /// `merge_request.*`). With `budget`, the work items stop once their
    /// merge requests would exceed it (at least one is kept) — decided
    /// before the check runs are read, so runs of merge requests that do
    /// not fit are never fetched (I1d review).
    pub async fn dev_status_of(
        &mut self,
        visible: &VisibleSet,
        items: &[i64],
        state: MrStateFilter,
        budget: Option<usize>,
    ) -> Result<DevStatusRead, ForgeError> {
        const QUERY: &str = "feed.dev_status.merge_requests";
        let mut out: BTreeMap<i64, DevItemStatus> = BTreeMap::new();
        let mut cut_after: Option<i64> = None;
        if items.is_empty() || !visible.shows_merge_requests() {
            return Ok(DevStatusRead::default());
        }
        let integration = visible.integration().get();
        let projects = visible.project_ids();
        let show_approvals = visible.shows_approvals();
        let show_checks = visible.shows_checks();
        let cap = i64::try_from(MAX_MERGE_REQUESTS_PER_ITEM)
            .unwrap_or(i64::MAX)
            .saturating_add(1);
        let db = self.db.clone();
        let conn = self.conn();
        db.within(QUERY, async {
            let rows: Vec<MrRow> = sqlx::query_as(&format!(
                "SELECT x.work_item_id, x.id, x.project_id, n.full_path, x.iid, x.title, x.state, \
                   x.draft, x.source_branch, x.target_branch, x.head_sha, \
                   (SELECT to_char(max(g.finished_at) AT TIME ZONE 'UTC', {TS}) \
                      FROM forge.merge_request_merges g \
                     WHERE g.merge_request_id = x.id AND g.state = 'applied'), \
                   to_char(x.closed_at AT TIME ZONE 'UTC', {TS}), \
                   (SELECT count(*) FROM forge.merge_request_approvals a \
                     WHERE a.merge_request_id = x.id AND a.revoked_at IS NULL \
                       AND a.head_sha = x.head_sha AND a.target_branch = x.target_branch), \
                   ROW(x.link_source, to_char(x.linked_at AT TIME ZONE 'UTC', {TS}), \
                     to_char(x.created_at AT TIME ZONE 'UTC', {TS}), \
                     to_char(x.updated_at AT TIME ZONE 'UTC', {TS})) \
                 FROM ( \
                   SELECT l.work_item_id, l.source AS link_source, l.linked_at, m.*, \
                     row_number() OVER (PARTITION BY l.work_item_id ORDER BY m.id DESC) AS rank \
                   FROM forge.merge_request_work_items l \
                   JOIN forge.work_items w ON w.id = l.work_item_id \
                   JOIN forge.merge_requests m ON m.id = l.merge_request_id \
                   JOIN forge.integration_bindings b ON b.project_id = m.project_id \
                     AND b.integration_id = w.integration_id \
                     AND b.external_project_id = w.external_project_id \
                   WHERE l.work_item_id = ANY($1) AND l.unlinked_at IS NULL \
                     AND w.integration_id = $2 AND m.project_id = ANY($3) \
                     AND ($4::text IS NULL OR m.state = $4) \
                 ) AS x \
                 LEFT JOIN forge.namespaces n ON n.kind = 'project' AND n.owner_id = x.project_id \
                 WHERE x.rank <= $5 \
                 ORDER BY x.work_item_id, x.id DESC"
            ))
            .bind(items)
            .bind(integration)
            .bind(&projects)
            .bind(state.state())
            .bind(cap)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
            // The budget (pages): the work items whose merge requests fit,
            // in id order, at least one — decided before any check run.
            let keep = match budget {
                None => None,
                Some(budget) => {
                    let mut per_item: BTreeMap<i64, usize> = BTreeMap::new();
                    for row in &rows {
                        *per_item.entry(row.0).or_default() += 1;
                    }
                    let mut listed = 0usize;
                    let mut last = None;
                    let mut cut = false;
                    for id in items {
                        let n = per_item
                            .get(id)
                            .map_or(0, |n| (*n).min(MAX_MERGE_REQUESTS_PER_ITEM));
                        if last.is_some() && listed + n > budget {
                            cut = true;
                            break;
                        }
                        listed += n;
                        last = Some(*id);
                    }
                    if cut {
                        cut_after = last;
                    }
                    cut_after
                }
            };
            let mut heads: Vec<(i64, String)> = Vec::new();
            for row in rows {
                if keep.is_some_and(|last| row.0 > last) {
                    continue;
                }
                let (
                    item,
                    id,
                    project,
                    path,
                    iid,
                    title,
                    mr_state,
                    draft,
                    source,
                    target,
                    head,
                    merged_at,
                    closed_at,
                    approvals,
                    (link_source, linked_at, created_at, updated_at),
                ) = row;
                let entry = out.entry(item).or_default();
                if entry.merge_requests.len() >= MAX_MERGE_REQUESTS_PER_ITEM {
                    entry.truncated = true;
                    continue;
                }
                if let (true, Some(head)) = (show_checks, &head) {
                    heads.push((project, head.clone())); // clone: the run lookup owns its keys
                }
                entry.merge_requests.push(DevMergeRequest {
                    project_id: ProjectId::new(project),
                    project_path: path,
                    iid,
                    title,
                    state: mr_state,
                    draft,
                    source_branch: branch(source),
                    target_branch: branch(target),
                    head_sha: head,
                    created_at,
                    updated_at,
                    merged_at,
                    closed_at,
                    approvals_given: if show_approvals {
                        Some(
                            u32::try_from(approvals)
                                .map_err(|_| invalid("dev_status_approvals_invalid", id))?,
                        )
                    } else {
                        None
                    },
                    checks: show_checks.then(Vec::new),
                    link_source,
                    linked_at,
                });
            }
            heads.sort();
            heads.dedup();
            if heads.is_empty() {
                return Ok(DevStatusRead {
                    statuses: out,
                    cut_after,
                });
            }
            let (head_projects, head_shas): (Vec<i64>, Vec<String>) = heads.into_iter().unzip();
            let runs: Vec<(i64, String, String, String, Option<String>)> = sqlx::query_as(
                "SELECT c.project_id, c.head_sha, c.name, c.status, c.conclusion \
                 FROM forge.check_runs c \
                 JOIN unnest($1::bigint[], $2::text[]) AS h(project_id, head_sha) \
                   ON c.project_id = h.project_id AND c.head_sha = h.head_sha \
                 ORDER BY c.project_id, c.head_sha, c.name",
            )
            .bind(&head_projects)
            .bind(&head_shas)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
            let mut by_head: BTreeMap<(i64, String), Vec<DevCheckRun>> = BTreeMap::new();
            for (project, head, name, status, conclusion) in runs {
                let bad = || {
                    ForgeError::new(ErrorCode::CheckRunDataInvalid)
                        .with_context("reason", "stored_status_invalid")
                        .with_context("project_id", project)
                };
                let status = CheckStatus::parse(&status).ok_or_else(bad)?;
                let conclusion = conclusion
                    .map(|c| CheckConclusion::parse(&c).ok_or_else(bad))
                    .transpose()?;
                by_head
                    .entry((project, head))
                    .or_default()
                    .push(DevCheckRun {
                        name,
                        status,
                        conclusion,
                    });
            }
            for status in out.values_mut() {
                for mr in &mut status.merge_requests {
                    if let (Some(checks), Some(head)) = (mr.checks.as_mut(), &mr.head_sha) {
                        if let Some(runs) = by_head.get(&(mr.project_id.get(), head.clone())) {
                            checks.clone_from(runs);
                        }
                    }
                }
            }
            Ok(DevStatusRead {
                statuses: out,
                cut_after,
            })
        })
        .await
    }
}
