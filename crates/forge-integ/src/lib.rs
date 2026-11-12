//! forge-integ: integrations (M3 I1c; plan 11 §5 I1; plan 02 §5.7, §11.2;
//! ADR-0016 §7 and its Amendment I1c). A feature crate: forge-web /
//! forge-cli → forge-integ → forge-auth / forge-db → forge-core →
//! forge-errors; it runs no SQL of its own and depends on no other feature
//! crate.
//!
//! [`Feed`] answers an integration's event feed (`GET
//! /api/v1/integrations/{id}/events`) and its head cursor (`…/events/cursor`)
//! on the route's proof (`Authorized<IntegrationFeedCtx>`: only the
//! integration's own service account reaches it):
//!
//! 1. one snapshot (`forge_db::feed::open`): the feed state, the decision's
//!    material, the events;
//! 2. `forge_auth::prove_visible` decides every bound project × visibility
//!    class with `authorize()` → the [`VisibleSet`] (digest with the binding
//!    generations);
//! 3. the cursor (`FeedPosition`, AES-256-GCM, at most 1.5 × the retention
//!    period old) is checked against the state and the set: `FG-INT-0008`
//!    (400) for a cursor that is not this integration's, `FG-INT-0007` (410)
//!    with `reason` `retention` / `epoch_mismatch` / `ahead_of_head` /
//!    `visible_set_changed` — never silently continued;
//! 4. the page: after the cursor up to `min(high_seq, work-item-sync)`, at
//!    most `limit` (≤ 500) events and 2 MiB; `next_cursor` is the last
//!    event returned when more remain, else the bound (it advances to the
//!    head when nothing visible is left; sealed, so it reveals nothing);
//! 5. the read record (`integration_feed_reads`, once a minute) and, when
//!    the consumer is behind half the retention period, `FG-INT-0009`
//!    (warn) in the log — a failure to record is logged and the page still
//!    goes out.
//!
//! One read (or head cursor) per integration is in flight at a time: a
//! concurrent one is `FG-WEB-0010` 429 `reason=feed_read_in_flight` with
//! `Retry-After: 1` (per process; review I1c).
//!
//! Only forge-web's feed routes call [`Feed::read_events`] and
//! [`Feed::head_cursor`], and only its development-status routes call
//! [`Feed::work_item_dev_status`] and [`Feed::dev_status_page`] (M3 I1d,
//! `dev_status.rs`; `tests/decided_callers.rs`).

use std::sync::Arc;
use std::time::Duration;

use forge_auth::{
    prove_visible, Action, Authorized, Credential, IntegrationFeedCtx, LoadedIntegrationFeed, Scope,
};
use forge_core::{telemetry, CursorKeys, FeedPosition, IntegrationId, RequestId, UserId};
use forge_db::feed::{self as db_feed, FeedEvent, FeedRead, FeedSnapshot, FeedState};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

mod act_as;
mod dev_status;
// ── M3 I2: webhooks (ADR-0016 §8) ──
pub mod webhooks;
// ── end M3 I2 ──
// ── M3 O2: the push mirror's administration ──
pub mod mirror;
// ── end M3 O2 ──

pub use act_as::{ActAs, ActAsParts, ClaimAnswer, SWEEP_INTERVAL};
/// What a claimed assertion is bound to (M3 X3: a merge request or a binding).
pub use forge_db::act_as::AssertedSubject as ClaimedSubject;

pub use dev_status::{
    checks_state, CheckConclusion, CheckStatus, DevMergeRequest, DevPerson, DevStatus, PageQuery,
    WorkItemQuery, WorkItemStatus, DEFAULT_DEV_STATUS_LIMIT,
};
pub use forge_db::feed::{DevCheckRun, MrStateFilter, MAX_DEV_STATUS_LIMIT};

/// The format version of the feed's answers (docs/api/integration-feed.md:
/// fields are only ever added).
pub const SCHEMA_VERSION: u32 = 1;
pub use forge_db::feed::{DEFAULT_LIMIT, MAX_LIMIT, MAX_PAGE_BYTES};

/// The feed service (`forge serve` builds one; forge-web's `AppState`).
pub struct Feed {
    db: Db,
    keys: Arc<CursorKeys>,
    in_flight: InFlight,
}

/// The integrations with a read in progress (review I1c, security MEDIUM:
/// one snapshot, up to 200 access checks and a 2 MiB page per integration
/// at a time; a second concurrent read is refused at once, never queued).
#[derive(Default)]
struct InFlight(std::sync::Mutex<std::collections::HashSet<IntegrationId>>);

/// A held slot; released on drop (also when a timeout drops the read).
struct Slot<'a> {
    set: &'a InFlight,
    integration: IntegrationId,
}

impl InFlight {
    fn guard(&self) -> std::sync::MutexGuard<'_, std::collections::HashSet<IntegrationId>> {
        // A panic while held cannot leave the set inconsistent (insert and
        // remove only): keep using it.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The slot of `integration`, or `FG-WEB-0010 reason=feed_read_in_flight`
    /// (429, `Retry-After: 1`).
    fn enter(&self, integration: IntegrationId) -> Result<Slot<'_>, ForgeError> {
        if !self.guard().insert(integration) {
            return Err(ForgeError::new(ErrorCode::RateLimited)
                .with_context("reason", "feed_read_in_flight")
                .with_context("integration_id", integration.get())
                .with_context("retry_after_s", 1));
        }
        Ok(Slot {
            set: self,
            integration,
        })
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.set.guard().remove(&self.integration);
    }
}

impl std::fmt::Debug for Feed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Feed").finish_non_exhaustive()
    }
}

/// A user an event names, with the personal facts the token may see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorUser {
    pub id: UserId,
    pub username: Option<String>,
    pub display_name: Option<String>,
    /// Only for a token holding `user_email`.
    pub email: Option<String>,
    /// Only for a token holding `user_employee_number`.
    pub employee_number: Option<String>,
}

/// Who caused an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    pub kind: String,
    pub user: Option<ActorUser>,
    pub via: Option<ActorUser>,
}

/// One work item of an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItemOut {
    pub provider: String,
    pub project_id: String,
    pub reference: String,
}

/// One event of a page.
#[derive(Debug, Clone, PartialEq)]
pub struct EventOut {
    /// `evt_<32 hex>`.
    pub id: String,
    /// The position right after this event (resume here once it is done).
    pub cursor: String,
    pub event_type: String,
    pub schema_version: i16,
    pub occurred_at: String,
    pub project_id: i64,
    pub actor: Actor,
    pub work_items: Vec<WorkItemOut>,
    pub data: serde_json::Value,
}

/// One page of the feed.
#[derive(Debug, Clone, PartialEq)]
pub struct EventsPage {
    pub events: Vec<EventOut>,
    pub next_cursor: String,
    pub has_more: bool,
}

/// What the request asks for (parsed and bounded by the route).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedQuery {
    pub cursor: Option<String>,
    /// 1 ..= [`MAX_LIMIT`].
    pub limit: usize,
}

fn internal(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", reason)
}

/// `FG-INT-0007` with its `reason`.
fn expired(reason: &'static str, integration: IntegrationId) -> ForgeError {
    ForgeError::new(ErrorCode::FeedCursorExpired)
        .with_context("reason", reason)
        .with_context("integration_id", integration.get())
}

/// What one request decided in its snapshot.
struct Decided {
    snapshot: FeedSnapshot,
    state: FeedState,
    visible: forge_core::VisibleSet,
}

impl Feed {
    pub fn new(db: Db, keys: Arc<CursorKeys>) -> Self {
        Self {
            db,
            keys,
            in_flight: InFlight::default(),
        }
    }

    /// The route loader's facts: the integration of the path (`None`:
    /// absent — the route answers the same 404 as for another
    /// integration).
    pub async fn target(
        &self,
        integration: IntegrationId,
    ) -> Result<Option<LoadedIntegrationFeed>, ForgeError> {
        Ok(forge_db::integrations::get(&self.db, integration)
            .await?
            .map(|i| LoadedIntegrationFeed {
                integration: i.id,
                service_account: i.service_account,
                enabled: i.enabled,
            }))
    }

    async fn decide(&self, proof: &Authorized<IntegrationFeedCtx>) -> Result<Decided, ForgeError> {
        let integration = *proof.id();
        let mut snapshot = db_feed::open(&self.db).await?;
        let decided = async {
            let state = match snapshot.state().await? {
                Some(state) => state,
                // The sequencer has not run yet: nothing is numbered.
                None => FeedState {
                    epoch: [0; 16],
                    high_seq: 0,
                    retained_from: 1,
                    work_items_seq: 0,
                    retention_days: snapshot.retention_days().await?,
                },
            };
            let material = snapshot
                .material(integration)
                .await?
                .ok_or_else(|| internal("feed_integration_vanished"))?;
            let visible = prove_visible(proof, &material)?;
            Ok::<_, ForgeError>((state, visible))
        }
        .await;
        match decided {
            Ok((state, visible)) => Ok(Decided {
                snapshot,
                state,
                visible,
            }),
            Err(e) => {
                snapshot.close().await;
                Err(e)
            }
        }
    }

    fn position(&self, d: &Decided, integration: IntegrationId, seq: i64) -> FeedPosition {
        FeedPosition {
            integration,
            epoch: d.state.epoch,
            visible_set: d.visible.digest(),
            seq,
        }
    }

    /// `GET /api/v1/integrations/{id}/events/cursor`: the position of the
    /// feed's head for this integration (resync: take it first, then the
    /// development status, then read from it).
    pub async fn head_cursor(
        &self,
        proof: &Authorized<IntegrationFeedCtx>,
    ) -> Result<String, ForgeError> {
        if proof.action() != Action::IntegrationEventCursorRead {
            return Err(internal("proof_action_mismatch"));
        }
        let _slot = self.in_flight.enter(*proof.id())?;
        let d = self.decide(proof).await?;
        let cursor = self
            .position(&d, *proof.id(), d.state.bound())
            .seal(&self.keys);
        d.snapshot.close().await;
        cursor
    }

    /// `GET /api/v1/integrations/{id}/events`: one page after `query.cursor`
    /// (from the oldest retained event without one).
    pub async fn read_events(
        &self,
        proof: &Authorized<IntegrationFeedCtx>,
        request_id: &RequestId,
        query: &FeedQuery,
    ) -> Result<EventsPage, ForgeError> {
        if proof.action() != Action::IntegrationEventRead {
            return Err(internal("proof_action_mismatch"));
        }
        if query.limit == 0 || query.limit > MAX_LIMIT {
            return Err(internal("feed_limit_out_of_range"));
        }
        let integration = *proof.id();
        let _slot = self.in_flight.enter(integration)?;
        let mut d = self.decide(proof).await?;
        let read = self.page(&mut d, integration, query).await;
        let (page, record) = match read {
            Ok(done) => done,
            Err(e) => {
                d.snapshot.close().await;
                return Err(e);
            }
        };
        let retention_days = d.state.retention_days;
        d.snapshot.close().await;
        let page = self.with_personal_facts(proof, page).await?;
        self.record(request_id, &record, retention_days).await;
        Ok(page)
    }

    /// The cursor's position checked against the state and the set.
    fn after(
        &self,
        d: &Decided,
        integration: IntegrationId,
        cursor: &str,
    ) -> Result<i64, ForgeError> {
        let days = u64::try_from(d.state.retention_days.max(1)).unwrap_or(1);
        let max_age = Duration::from_secs(days * 86_400 * 3 / 2);
        let position = FeedPosition::open(&self.keys, integration, cursor, max_age)?;
        self.check_position(d, integration, &position)
    }

    /// A sealed position checked against the state and the set, in the
    /// order epoch → `ahead_of_head` → `retention` → `visible_set_changed`
    /// (the feed's cursor and the development status's page cursor alike).
    fn check_position(
        &self,
        d: &Decided,
        integration: IntegrationId,
        position: &FeedPosition,
    ) -> Result<i64, ForgeError> {
        if position.epoch != d.state.epoch {
            return Err(expired("epoch_mismatch", integration));
        }
        if position.seq > d.state.high_seq {
            return Err(expired("ahead_of_head", integration)
                .with_context("cursor_seq", position.seq)
                .with_context("high_seq", d.state.high_seq));
        }
        if position.seq + 1 < d.state.retained_from {
            return Err(expired("retention", integration)
                .with_context("cursor_seq", position.seq)
                .with_context("retained_from", d.state.retained_from));
        }
        if position.visible_set != d.visible.digest() {
            return Err(expired("visible_set_changed", integration));
        }
        Ok(position.seq)
    }

    async fn page(
        &self,
        d: &mut Decided,
        integration: IntegrationId,
        query: &FeedQuery,
    ) -> Result<(EventsPage, FeedRead), ForgeError> {
        let after = match query.cursor.as_deref() {
            Some(cursor) => self.after(d, integration, cursor)?,
            None => d.state.retained_from.saturating_sub(1).max(0),
        };
        let bound = d.state.bound();
        let page = d
            .snapshot
            .read_feed_page(&d.visible, after, bound, query.limit, MAX_PAGE_BYTES)
            .await?;
        let next = match (page.has_more, page.events.last()) {
            (true, Some(last)) => last.seq,
            // Nothing visible is left: up to the head (never backwards).
            _ => bound.max(after),
        };
        let oldest_unread_at = d.snapshot.oldest_unread_at(&d.visible, next, bound).await?;
        let mut events = Vec::with_capacity(page.events.len());
        for event in page.events {
            let cursor = self.position(d, integration, event.seq).seal(&self.keys)?;
            events.push(event_out(event, cursor));
        }
        let next_cursor = self.position(d, integration, next).seal(&self.keys)?;
        Ok((
            EventsPage {
                events,
                next_cursor,
                has_more: page.has_more,
            },
            FeedRead {
                integration,
                cursor_seq: next,
                bound: bound.max(next),
                oldest_unread_at,
            },
        ))
    }

    /// Add e-mail addresses and employee numbers for a token holding
    /// `user_email` / `user_employee_number` (read on the write pool; the
    /// read pool never sees them).
    async fn with_personal_facts(
        &self,
        proof: &Authorized<IntegrationFeedCtx>,
        mut page: EventsPage,
    ) -> Result<EventsPage, ForgeError> {
        let (email, employee) = personal_scopes(proof);
        if !(email || employee) {
            return Ok(page);
        }
        let mut users: Vec<UserId> = page
            .events
            .iter()
            .flat_map(|e| [e.actor.user.as_ref(), e.actor.via.as_ref()])
            .flatten()
            .map(|u| u.id)
            .collect();
        users.sort();
        users.dedup();
        let facts = db_feed::personal_facts(&self.db, &users).await?;
        for event in &mut page.events {
            for user in [event.actor.user.as_mut(), event.actor.via.as_mut()]
                .into_iter()
                .flatten()
            {
                if let Some((mail, number)) = facts.get(&user.id) {
                    if email {
                        user.email.clone_from(mail);
                    }
                    if employee {
                        user.employee_number.clone_from(number);
                    }
                }
            }
        }
        Ok(page)
    }

    /// The read record (once a minute) and the lag warning. A failure is
    /// logged once here; the page still goes out.
    async fn record(&self, request_id: &RequestId, read: &FeedRead, retention_days: i32) {
        match db_feed::record_read(&self.db, read).await {
            Ok(true) => {
                if let Some(lagging) = behind(read, retention_days) {
                    telemetry::log_failure(
                        &db_feed::lag_error(&lagging, retention_days),
                        request_id,
                    );
                }
            }
            Ok(false) => {}
            Err(e) => telemetry::log_failure(
                &e.with_context("integration_id", read.integration.get()),
                request_id,
            ),
        }
    }
}

/// Whether the feed's token may see e-mail addresses (`user_email`) and
/// employee numbers (`user_employee_number`) — the one rule for every
/// person the feed and the development status name.
fn personal_scopes(proof: &Authorized<IntegrationFeedCtx>) -> (bool, bool) {
    let scopes: &[Scope] = match proof.principal().credential() {
        Credential::Token(scopes) => scopes,
        // The feed's principal always holds a token (a service account
        // never has a session); a session sees nothing personal.
        Credential::Session => &[],
        // Refused on the feed (ADR-0025 §4); nothing personal.
        Credential::ActAs(_) => &[],
        // Refused on the feed (ADR-0029 §8); nothing personal.
        Credential::Runner(_) | Credential::Job(_) => &[],
    };
    (
        scopes.contains(&Scope::UserEmail),
        scopes.contains(&Scope::UserEmployeeNumber),
    )
}

/// The lag of a read whose oldest unread event is older than half the
/// retention period (`FG-INT-0009 reason=behind`).
fn behind(read: &FeedRead, retention_days: i32) -> Option<db_feed::Lagging> {
    let oldest = read.oldest_unread_at.as_deref()?;
    let age = age_seconds(oldest)?;
    let limit = i64::from(retention_days) * 86_400 / 2;
    (age > limit).then_some(db_feed::Lagging {
        integration: read.integration,
        reason: db_feed::LagReason::Behind,
        last_read_at: None,
        behind_seconds: Some(age),
    })
}

/// Seconds since `stamp` (`YYYY-MM-DDTHH:MM:SS[.mmm]Z`, UTC); `None` when
/// it does not parse or the clock is before it.
fn age_seconds(stamp: &str) -> Option<i64> {
    let digits =
        |from: usize, len: usize| -> Option<i64> { stamp.get(from..from + len)?.parse().ok() };
    let (y, mo, d) = (digits(0, 4)?, digits(5, 2)?, digits(8, 2)?);
    let (h, mi, s) = (digits(11, 2)?, digits(14, 2)?, digits(17, 2)?);
    // Days from the civil date (Howard Hinnant's algorithm).
    let (y, m) = if mo <= 2 {
        (y - 1, mo + 9)
    } else {
        (y, mo - 3)
    };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let then = days * 86_400 + h * 3_600 + mi * 60 + s;
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs(),
    )
    .ok()?;
    (now >= then).then_some(now - then)
}

fn user_out(user: Option<db_feed::FeedUser>) -> Option<ActorUser> {
    user.map(|u| ActorUser {
        id: u.id,
        username: u.username,
        display_name: u.display_name,
        email: None,
        employee_number: None,
    })
}

fn event_out(event: FeedEvent, cursor: String) -> EventOut {
    EventOut {
        id: format!("evt_{}", event.event_id.replace('-', "")),
        cursor,
        event_type: event.event_type,
        schema_version: event.schema_version,
        occurred_at: event.occurred_at,
        project_id: event.project_id.get(),
        actor: Actor {
            kind: event.actor.kind,
            user: user_out(event.actor.user),
            via: user_out(event.actor.via),
        },
        work_items: event
            .work_items
            .into_iter()
            .map(|w| WorkItemOut {
                provider: w.provider,
                project_id: w.project_id,
                reference: w.reference,
            })
            .collect(),
        data: event.payload,
    }
}

#[cfg(test)]
mod unit {
    use super::age_seconds;

    #[test]
    fn age_is_read_from_utc_stamps() {
        let age = age_seconds("2000-01-01T00:00:00Z").expect("past");
        // More than 25 years ago, less than 200.
        assert!(age > 25 * 365 * 86_400 && age < 200 * 365 * 86_400, "{age}");
        assert!(age_seconds("2000-01-01T00:00:00.123Z").is_some());
        assert_eq!(age_seconds("3000-01-01T00:00:00Z"), None);
        assert_eq!(age_seconds("garbage"), None);
    }
}
