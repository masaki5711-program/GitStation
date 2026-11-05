//! The event feed of an integration (M3 I1c; plan 02 §5.7, §11.2; ADR-0016
//! §7 and its Amendment I1c): what the feed's decision reads
//! ([`FeedMaterial`]), the events it may show ([`VisibleSet`]) and the
//! position a cursor seals ([`FeedPosition`]).
//!
//! - **The decision stays in forge-auth.** forge-db reads the material in
//!   the feed's snapshot; `forge_auth::prove_visible` decides every bound
//!   project × visibility class with `authorize()` and builds the
//!   [`VisibleSet`] through its one constructor
//!   [`VisibleSet::from_authorized_decisions`] (a gate test keeps every
//!   other caller out: forge-integ `tests/decided_callers.rs`). forge-db's
//!   read takes the set and turns only its project ids and event types into
//!   SQL; no role or membership is decided in SQL.
//! - **The digest names the decision.** A cursor carries the first 128 bits
//!   of the SHA-256 of the set's canonical form — the integration, the
//!   event types the allowlist admits, and every visible project **with its
//!   binding generation** (`integration_bindings.generation`, new for every
//!   bind). When anything the set depends on changes (a binding, the
//!   service account's role, a project's deletion, the allowlist), the
//!   digest changes and the cursor answers 410 `visible_set_changed`. The
//!   generation makes an unbind followed by a rebind of the same project a
//!   change too (I1b ends that project's links on unbind without
//!   `work_item.unlinked` events, so the consumer must resync, ADR-0016
//!   Amendment I1b §5 and its I1c dependency).
//! - **The cursor** is [`crate::cursor`]'s AES-256-GCM sealing with purpose
//!   [`CURSOR_PURPOSE`]; the payload is `version:u8 = 1 | integration:u64 |
//!   epoch:[u8; 16] | visible_set:[u8; 16] | seq:u64` (big endian). Every
//!   failure is `FG-INT-0008` with a `rule` (another integration's cursor:
//!   `other_integration`) — except an authentic cursor older than the
//!   caller's `max_age`, which is `FG-INT-0007 reason=retention`. The value
//!   is never logged.

use sha2::{Digest, Sha256};
use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};

use crate::cursor::CursorKeys;
use crate::event_type::{EventType, VisibilityClass};
use crate::ids::{IntegrationId, ProjectId, UserId};
use crate::integration::ALL_EVENT_TYPES;
use crate::namespace_resolver::ProjectAccess;

/// The purpose a feed cursor is sealed for ([`CursorKeys::encode`]).
pub const CURSOR_PURPOSE: &str = "integration.events";
/// The purpose a development-status page cursor is sealed for (M3 I1d):
/// another purpose than the feed's, so neither passes for the other.
pub const DEV_STATUS_CURSOR_PURPOSE: &str = "integration.dev_status";
/// The bytes of a digest and of an epoch.
pub const DIGEST_BYTES: usize = 16;
const PAYLOAD_VERSION: u8 = 1;
const PAYLOAD_BYTES: usize = 1 + 8 + DIGEST_BYTES + DIGEST_BYTES + 8;

/// One bound project as the feed's decision reads it (in the feed's
/// snapshot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedBinding {
    pub project: ProjectId,
    /// `integration_bindings.generation`: new for every bind.
    pub generation: i64,
    /// The service account's access facts on the project (`None`: the
    /// project row is gone — it is then not visible).
    pub access: Option<ProjectAccess>,
}

/// What `forge_auth::prove_visible` decides on: the integration as stored
/// and its bindings, read in one snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedMaterial {
    pub integration: IntegrationId,
    pub service_account: UserId,
    pub enabled: bool,
    /// The allowlist as stored (`*`, a type or `<family>.*`).
    pub event_types: Vec<String>,
    /// By project id.
    pub bindings: Vec<FeedBinding>,
}

/// One bound project whose integration-class events the feed shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VisibleProject {
    pub project: ProjectId,
    pub generation: i64,
}

/// The events an integration's feed may show in one request: the bound
/// projects `authorize()` allowed for the integration class, narrowed by
/// the allowlist. Built only by `forge_auth::prove_visible`; used for one
/// request and never stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleSet {
    integration: IntegrationId,
    /// Sorted by project, without duplicates.
    projects: Vec<VisibleProject>,
    /// Sorted by name.
    event_types: Vec<EventType>,
    digest: [u8; DIGEST_BYTES],
}

impl VisibleSet {
    /// The only constructor: `allowed` are the bound projects (with their
    /// binding generation) that `authorize()` allowed for the integration
    /// class; `allowlist` is the integration's `event_types`. Callers
    /// outside forge-auth are refused by a gate test.
    pub fn from_authorized_decisions(
        integration: IntegrationId,
        mut allowed: Vec<VisibleProject>,
        allowlist: &[String],
    ) -> Self {
        allowed.sort();
        allowed.dedup();
        let event_types = allowed_event_types(allowlist);
        let digest = digest_of(integration, &allowed, &event_types);
        Self {
            integration,
            projects: allowed,
            event_types,
            digest,
        }
    }

    pub fn integration(&self) -> IntegrationId {
        self.integration
    }

    pub fn projects(&self) -> &[VisibleProject] {
        &self.projects
    }

    /// The visible projects' ids (the SQL filter).
    pub fn project_ids(&self) -> Vec<i64> {
        self.projects.iter().map(|p| p.project.get()).collect()
    }

    /// The event types shown (the SQL filter), by name.
    pub fn event_type_names(&self) -> Vec<&'static str> {
        self.event_types.iter().map(|t| t.as_str()).collect()
    }

    /// The first 128 bits of the SHA-256 of the canonical form.
    pub fn digest(&self) -> [u8; DIGEST_BYTES] {
        self.digest
    }

    /// No event can be visible (no project or no admitted type).
    pub fn shows_nothing(&self) -> bool {
        self.projects.is_empty() || self.event_types.is_empty()
    }

    /// M3 I1d: the development status shows merge requests (some visible
    /// project, and the allowlist admits a `merge_request.*` type).
    /// I1d review: a merge request's check runs (with merge requests, when
    /// the allowlist admits `check.completed`).
    pub fn shows_checks(&self) -> bool {
        self.shows_merge_requests() && self.event_types.contains(&EventType::CheckCompleted)
    }

    /// I1d review: a merge request's approvals (with merge requests, when
    /// the allowlist admits `merge_request.approved` or `.unapproved`).
    pub fn shows_approvals(&self) -> bool {
        self.shows_merge_requests()
            && (self.event_types.contains(&EventType::MergeRequestApproved)
                || self
                    .event_types
                    .contains(&EventType::MergeRequestUnapproved))
    }

    pub fn shows_merge_requests(&self) -> bool {
        !self.projects.is_empty()
            && self
                .event_types
                .iter()
                .any(|t| t.as_str().starts_with("merge_request."))
    }
}

/// The integration-class event types `allowlist` admits (`*` every one,
/// `<family>.*` those of the family, a name itself), sorted by name.
/// Entries of another class or unknown names admit nothing (the CHECK and
/// `integration::event_type_entry_ok` refuse them when stored).
pub fn allowed_event_types(allowlist: &[String]) -> Vec<EventType> {
    let admits = |t: EventType| {
        allowlist.iter().any(|entry| {
            entry == ALL_EVENT_TYPES
                || entry == t.as_str()
                || entry.strip_suffix(".*").is_some_and(|family| {
                    t.as_str()
                        .strip_prefix(family)
                        .is_some_and(|rest| rest.starts_with('.'))
                })
        })
    };
    let mut types: Vec<EventType> = EventType::ALL
        .into_iter()
        .filter(|t| t.visibility_class() == VisibilityClass::Integration)
        .filter(|t| admits(*t))
        .collect();
    types.sort_by_key(|t| t.as_str());
    types
}

fn digest_of(
    integration: IntegrationId,
    projects: &[VisibleProject],
    types: &[EventType],
) -> [u8; DIGEST_BYTES] {
    let mut canonical = format!("forge-feed-visible-set/1\nintegration={integration}\ntypes=");
    let names: Vec<&str> = types.iter().map(|t| t.as_str()).collect();
    canonical.push_str(&names.join(","));
    canonical.push_str("\nprojects=");
    let projects: Vec<String> = projects
        .iter()
        .map(|p| format!("{}:{}", p.project, p.generation))
        .collect();
    canonical.push_str(&projects.join(","));
    let full = Sha256::digest(canonical.as_bytes());
    let mut out = [0u8; DIGEST_BYTES];
    out.copy_from_slice(&full[..DIGEST_BYTES]);
    out
}

/// The position a feed cursor seals: the integration, the feed's epoch,
/// the digest of the set it was issued under and the last `seq` consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedPosition {
    pub integration: IntegrationId,
    pub epoch: [u8; DIGEST_BYTES],
    pub visible_set: [u8; DIGEST_BYTES],
    /// Every event with `seq <= self.seq` was consumed (0: none).
    pub seq: i64,
}

/// `FG-INT-0008` with `rule` (never the value).
fn invalid(rule: &str, integration: IntegrationId) -> ForgeError {
    ForgeError::new(ErrorCode::FeedCursorInvalid)
        .with_context("rule", rule)
        .with_context("integration_id", integration.get())
}

/// `cursor` opened for `purpose`: `FG-INT-0008` with the decoder's `rule`,
/// or `FG-INT-0007 reason=retention rule=expired` past `max_age`.
fn open_payload(
    keys: &CursorKeys,
    purpose: &str,
    integration: IntegrationId,
    cursor: &str,
    max_age: Duration,
) -> Result<Vec<u8>, ForgeError> {
    keys.decode(purpose, cursor, max_age).map_err(|e| {
        let rule = e
            .context()
            .iter()
            .find(|(k, _)| *k == "rule")
            .map_or("invalid", |(_, v)| v.as_str());
        if rule == "expired" {
            ForgeError::new(ErrorCode::FeedCursorExpired)
                .with_context("reason", "retention")
                .with_context("rule", "expired")
                .with_context("integration_id", integration.get())
        } else {
            invalid(rule, integration)
        }
    })
}

/// A non-negative `i64` as its big-endian `u64` (`FG-SYS-0001` otherwise).
fn unsigned(value: i64, field: &'static str) -> Result<[u8; 8], ForgeError> {
    u64::try_from(value).map(u64::to_be_bytes).map_err(|_| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "feed_position_negative")
            .with_context(field, value)
    })
}

impl FeedPosition {
    /// The payload bytes (the feed cursor's, and the head of a
    /// development-status cursor's).
    fn payload(&self) -> Result<Vec<u8>, ForgeError> {
        let mut payload = Vec::with_capacity(PAYLOAD_BYTES);
        payload.push(PAYLOAD_VERSION);
        payload.extend_from_slice(&unsigned(self.integration.get(), "integration_id")?);
        payload.extend_from_slice(&self.epoch);
        payload.extend_from_slice(&self.visible_set);
        payload.extend_from_slice(&unsigned(self.seq, "seq")?);
        Ok(payload)
    }

    /// The cursor text (sealed with the newest key).
    pub fn seal(&self, keys: &CursorKeys) -> Result<String, ForgeError> {
        keys.encode(CURSOR_PURPOSE, &self.payload()?)
    }

    /// Open a cursor `integration` was given. A malformed, tampered or
    /// foreign one is `FG-INT-0008` with `rule`; an authentic one older
    /// than `max_age` is `FG-INT-0007 reason=retention rule=expired`.
    pub fn open(
        keys: &CursorKeys,
        integration: IntegrationId,
        cursor: &str,
        max_age: Duration,
    ) -> Result<Self, ForgeError> {
        let payload = open_payload(keys, CURSOR_PURPOSE, integration, cursor, max_age)?;
        Self::parse(&payload, integration)
    }

    fn parse(payload: &[u8], integration: IntegrationId) -> Result<Self, ForgeError> {
        let bad = || invalid("payload", integration);
        if payload.len() != PAYLOAD_BYTES || payload.first() != Some(&PAYLOAD_VERSION) {
            return Err(bad());
        }
        let field = |from: usize, len: usize| payload.get(from..from + len).ok_or_else(bad);
        let u64_at = |from: usize| -> Result<u64, ForgeError> {
            let raw: [u8; 8] = field(from, 8)?.try_into().map_err(|_| bad())?;
            Ok(u64::from_be_bytes(raw))
        };
        let owner = i64::try_from(u64_at(1)?).map_err(|_| bad())?;
        let mut epoch = [0u8; DIGEST_BYTES];
        epoch.copy_from_slice(field(9, DIGEST_BYTES)?);
        let mut visible_set = [0u8; DIGEST_BYTES];
        visible_set.copy_from_slice(field(9 + DIGEST_BYTES, DIGEST_BYTES)?);
        let seq = i64::try_from(u64_at(9 + 2 * DIGEST_BYTES)?).map_err(|_| bad())?;
        if owner != integration.get() {
            return Err(invalid("other_integration", integration));
        }
        Ok(Self {
            integration,
            epoch,
            visible_set,
            seq,
        })
    }
}

/// Where a paged development-status read continues (M3 I1d,
/// `GET /api/v1/integrations/{id}/dev-status?cursor=…`): the feed position
/// its **first** page was read at (`as_of_seq`, with the epoch and the
/// visible set's digest — a later page checks them like the feed does)
/// and the last work item returned. Sealed for
/// [`DEV_STATUS_CURSOR_PURPOSE`]; the payload is the feed position's
/// followed by `after_work_item:u64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DevStatusPosition {
    pub feed: FeedPosition,
    /// Every work item with `id <= after_work_item` was returned.
    pub after_work_item: i64,
}

impl DevStatusPosition {
    /// The cursor text (sealed with the newest key).
    pub fn seal(&self, keys: &CursorKeys) -> Result<String, ForgeError> {
        let mut payload = self.feed.payload()?;
        payload.extend_from_slice(&unsigned(self.after_work_item, "after_work_item")?);
        keys.encode(DEV_STATUS_CURSOR_PURPOSE, &payload)
    }

    /// Open a page cursor `integration` was given (the errors of
    /// [`FeedPosition::open`]).
    pub fn open(
        keys: &CursorKeys,
        integration: IntegrationId,
        cursor: &str,
        max_age: Duration,
    ) -> Result<Self, ForgeError> {
        let payload = open_payload(
            keys,
            DEV_STATUS_CURSOR_PURPOSE,
            integration,
            cursor,
            max_age,
        )?;
        let bad = || invalid("payload", integration);
        if payload.len() != PAYLOAD_BYTES + 8 {
            return Err(bad());
        }
        let (head, tail) = payload.split_at(PAYLOAD_BYTES);
        let feed = FeedPosition::parse(head, integration)?;
        let raw: [u8; 8] = tail.try_into().map_err(|_| bad())?;
        let after_work_item = i64::try_from(u64::from_be_bytes(raw)).map_err(|_| bad())?;
        Ok(Self {
            feed,
            after_work_item,
        })
    }
}
