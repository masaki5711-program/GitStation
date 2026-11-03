//! Work items (M3 I1b, plan 02 §2, §5.8, ADR-0016 Amendment I1b): the
//! references to kanflow's product backlog items that forge detects in
//! merge request titles and branch names, and the audit details / event
//! payload of a link between a merge request and a work item.
//!
//! **The matcher** ([`find_refs`]) is the rule of plan 02 §2, the same as
//! kanflow's K1: `/(?<![a-z0-9])pbi[-_ ]?#?(\d{1,7})(?![0-9])/gi`, with
//! ASCII-only case folding (JavaScript without the `u` flag). The Rust
//! `regex` crate has no look-around, so it is written by hand; the shared
//! vectors `docs/api/pbi-ref-vectors.json` are the contract both sides
//! test against (`pbi_ref_vectors_match_shared_contract`). A reference is
//! `PBI-` + the number as a decimal integer (`PBI-007` is `PBI-7`); the
//! number is never cut short, so `PBI-430` is never `PBI-43`.
//!
//! A work item is identified by (integration, external project, ref)
//! (02 §2: `PBI-123` is numbered per kanflow project; which project is the
//! binding's).

use crate::event_type::EventType;
use crate::ids::{IntegrationId, MergeRequestId, ProjectId, UserId};
use crate::integration::IntegrationKind;

/// The most digits a reference number has (kanflow's `displayId`).
pub const MAX_REF_DIGITS: usize = 7;

/// A work item reference in its canonical form, `PBI-<n>` (`n` without
/// leading zeros; the CHECK `work_items_ref_format`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkItemRef(String);

impl WorkItemRef {
    fn of_number(n: u32) -> Self {
        Self(format!("PBI-{n}"))
    }

    /// `raw` in the canonical form only (`PBI-43`; not `pbi-43`, `PBI #43`
    /// or `PBI-043`): what an API path names. `None` for anything else.
    pub fn parse(raw: &str) -> Option<Self> {
        let digits = raw.strip_prefix("PBI-")?;
        let canonical = !digits.is_empty()
            && digits.len() <= MAX_REF_DIGITS
            && digits.bytes().all(|b| b.is_ascii_digit())
            && (digits == "0" || !digits.starts_with('0'));
        canonical.then(|| Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WorkItemRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Every distinct work item reference in `text`, in the order of its first
/// match (module docs; the shared vectors).
pub fn find_refs(text: &str) -> Vec<WorkItemRef> {
    let bytes = text.as_bytes();
    let mut found: Vec<WorkItemRef> = Vec::new();
    let mut at = 0;
    while let Some(start) = next_pbi(bytes, at) {
        match match_at(bytes, start) {
            Some((number, end)) => {
                let reference = WorkItemRef::of_number(number);
                if !found.contains(&reference) {
                    found.push(reference);
                }
                at = end;
            }
            None => at = start + 1,
        }
    }
    found
}

/// The next index at or after `from` where `pbi` (any ASCII case) starts.
fn next_pbi(bytes: &[u8], from: usize) -> Option<usize> {
    let tail = bytes.get(from..)?;
    tail.windows(3)
        .position(|w| w.eq_ignore_ascii_case(b"pbi"))
        .map(|offset| from + offset)
}

/// The number and the end of the match starting with `pbi` at `start`, if
/// the rule matches there. No backtracking is needed: the separator and
/// `#` are never digits, so taking an optional one never loses a match
/// that skipping it would find, and a digit run longer than
/// [`MAX_REF_DIGITS`] fails every shorter prefix on the look-ahead.
fn match_at(bytes: &[u8], start: usize) -> Option<(u32, usize)> {
    // (?<![a-z0-9]) with /i: an ASCII letter or digit before is no
    // boundary. UTF-8 continuation bytes are >= 0x80, never ASCII.
    if let Some(before) = start.checked_sub(1).and_then(|i| bytes.get(i)) {
        if before.is_ascii_alphanumeric() {
            return None;
        }
    }
    let mut i = start + 3;
    if matches!(bytes.get(i), Some(b'-' | b'_' | b' ')) {
        i += 1;
    }
    if bytes.get(i) == Some(&b'#') {
        i += 1;
    }
    let digits = bytes.get(i..).map_or(0, |rest| {
        rest.iter().take_while(|b| b.is_ascii_digit()).count()
    });
    if digits == 0 || digits > MAX_REF_DIGITS {
        return None;
    }
    let end = i + digits;
    let number = bytes
        .get(i..end)
        .and_then(|d| std::str::from_utf8(d).ok())
        .and_then(|d| d.parse::<u32>().ok())?;
    Some((number, end))
}

/// How a link between a merge request and a work item was made or ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkSource {
    /// Detected (title, source branch) by the `work-item-sync` subscriber.
    Auto,
    /// A person linked or unlinked it on the merge request.
    Manual,
}

impl LinkSource {
    pub const ALL: [LinkSource; 2] = [Self::Auto, Self::Manual];

    /// The stored value (CHECK `merge_request_work_items_source`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == raw)
    }
}

/// One work item as an event names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItem {
    pub id: i64,
    pub integration: IntegrationId,
    pub kind: IntegrationKind,
    pub external_project_id: String,
    pub reference: WorkItemRef,
}

/// A link of a merge request to a work item made or ended
/// (`work_item.linked` / `work_item.unlinked`, integration class). The
/// subscriber's are derived events (actor `system`); a person's carry an
/// audit row on the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkItemChange {
    /// `work_item.linked`: link row `link` was created by `source`.
    Linked {
        project: ProjectId,
        merge_request: MergeRequestId,
        iid: i64,
        link: i64,
        work_item: WorkItem,
        source: LinkSource,
        /// The person who linked it (`None` for a detected link).
        by: Option<UserId>,
    },
    /// `work_item.unlinked`: link row `link` ended by `source` (the title
    /// no longer names it, or a person unlinked it). The row stays.
    Unlinked {
        project: ProjectId,
        merge_request: MergeRequestId,
        iid: i64,
        link: i64,
        work_item: WorkItem,
        source: LinkSource,
        by: Option<UserId>,
    },
}

impl WorkItemChange {
    pub fn event_type(&self) -> EventType {
        match self {
            Self::Linked { .. } => EventType::WorkItemLinked,
            Self::Unlinked { .. } => EventType::WorkItemUnlinked,
        }
    }

    /// The `audit_log.action` (and event type name).
    pub fn action(&self) -> &'static str {
        self.event_type().as_str()
    }

    /// The dedup key's prefix (`<prefix>:<link_id>`: each link row is made
    /// once and ends once).
    pub fn dedup_prefix(&self) -> &'static str {
        match self {
            Self::Linked { .. } => "work_item_linked",
            Self::Unlinked { .. } => "work_item_unlinked",
        }
    }

    pub fn project(&self) -> ProjectId {
        match self {
            Self::Linked { project, .. } | Self::Unlinked { project, .. } => *project,
        }
    }

    pub fn merge_request(&self) -> MergeRequestId {
        match self {
            Self::Linked { merge_request, .. } | Self::Unlinked { merge_request, .. } => {
                *merge_request
            }
        }
    }

    pub fn link(&self) -> i64 {
        match self {
            Self::Linked { link, .. } | Self::Unlinked { link, .. } => *link,
        }
    }

    pub fn work_item(&self) -> &WorkItem {
        match self {
            Self::Linked { work_item, .. } | Self::Unlinked { work_item, .. } => work_item,
        }
    }

    /// `{"merge_request_id","iid","link_id","work_item_id","work_item":
    /// {"provider","project_id","ref"},"integration_id","source","by"}`
    /// (02 §5.7's `work_items` entry shape; ids and the reference only).
    pub fn to_value(&self) -> serde_json::Value {
        let (Self::Linked {
            merge_request,
            iid,
            link,
            work_item,
            source,
            by,
            ..
        }
        | Self::Unlinked {
            merge_request,
            iid,
            link,
            work_item,
            source,
            by,
            ..
        }) = self;
        serde_json::json!({
            "merge_request_id": merge_request.get(),
            "iid": iid,
            "link_id": link,
            "work_item_id": work_item.id,
            "work_item": {
                "provider": work_item.kind.as_str(),
                "project_id": work_item.external_project_id,
                "ref": work_item.reference.as_str(),
            },
            "integration_id": work_item.integration.get(),
            "source": source.as_str(),
            "by": by.map(UserId::get),
        })
    }
}
