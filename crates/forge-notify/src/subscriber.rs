//! The `notify` subscriber (ADR-0016 §7 "independent", ADR-0016 Amendment
//! N1): it reads the published events in order, one per transaction, and
//! records who is to be told.
//!
//! | event | told | when not |
//! |---|---|---|
//! | `merge_request.approved` | the merge request's author | the approver is the author |
//! | `merge_request.merged` | the author | the person who merged is the author |
//! | `merge_request.commented` | the author | the commenter is the author, or the event is a thread being resolved or reopened |
//! | `merge_request.conflict_predicted` (M3 X6) | the author of the event's merge request (each side of the pair has its own event) | — (the scan is the system's) |
//!
//! Every other event concerns nobody (decided in `NotificationKind::
//! from_event`, which matches every event type). A "review requested"
//! kind, mentions and a push-mirror failure join with the events that
//! announce them; none exists yet.
//!
//! One row per (event, recipient) is written in the step's own transaction
//! with the decision made right then ([`crate::access::decide`]): `pending`
//! with a `notification.send` job, or `skipped` / `suppressed` with the
//! reason, so "why did I get no mail?" always has an answer in the table.
//! The mail itself is the job's, never the subscriber's (no network call in
//! a step).
//!
//! In the same step the same recipient gets an in-app item (M3 U4,
//! `forge.in_app_notifications`, one per event and person) when
//! [`crate::access::decide_in_app`] says so: an active person who has not
//! switched the kind off in the app and may read the merge request now. No
//! row is written otherwise (the in-app list has no delivery to explain);
//! the list decides visibility again on every read.
//!
//! With `notify.enabled = false` the worker registers [`Notify::paused`]:
//! it reads every event and records no mail, only the in-app items (they
//! need no relay), so its cursor follows the head.
//!
//! An event whose payload this build cannot read fails the step (counted,
//! quarantined after five tries: `FG-DB-0008`); a type this build does not
//! know is passed over with a warning, never silently.

use std::time::Duration;

use forge_core::config::NotifyConfig;
use forge_core::ids::UserId;
use forge_core::{EventType, NotificationKind};
use forge_db::jobs::{JobTimeout, MaxAttempts};
use forge_db::notify::{self, in_app, Delivery, Initial, NewNotification};
use forge_db::subscribers::{DerivedTx, HandleFuture, Ordering, SourceEvent, Subscriber};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

use crate::access::{decide, decide_in_app, Decision};

/// The subscriber's name (its row in forge.event_subscribers).
pub const NAME: &str = "notify";

/// A job outlives its send by this much (database steps before and after).
const JOB_MARGIN: Duration = Duration::from_secs(15);

/// The subscriber.
pub struct Notify {
    db: Db,
    delivery: Delivery,
    /// `notify.enabled = false`: the subscriber stays registered and reads
    /// every event, but records no mail (see [`Notify::paused`]).
    paused: bool,
}

impl Notify {
    pub fn new(db: &Db, cfg: &NotifyConfig) -> Result<Self, ForgeError> {
        let internal = |reason: &'static str| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", reason)
                .with_context("subscriber", NAME)
        };
        Ok(Self {
            paused: false,
            db: db.clone(), // clone: a pool handle
            delivery: Delivery {
                max_attempts: MaxAttempts::new(cfg.max_attempts)
                    .ok_or_else(|| internal("max_attempts_out_of_range"))?,
                timeout: JobTimeout::new(cfg.send_timeout + JOB_MARGIN)
                    .ok_or_else(|| internal("send_timeout_out_of_range"))?,
            },
        })
    }
}

impl Notify {
    /// The subscriber while mail is off (`notify.enabled = false`). It keeps
    /// its row and its cursor moving — every event is read, the in-app
    /// items are recorded and no mail row is — so turning mail on later
    /// starts from the head, not from where mail was switched off (no mail
    /// for the whole disabled period), the subscriber lag stays zero and
    /// event retention is not held back.
    pub fn paused(db: &Db) -> Result<Self, ForgeError> {
        let mut paused = Self::new(db, &NotifyConfig::default())?;
        paused.paused = true;
        Ok(paused)
    }
}

fn bad_payload(event: &SourceEvent, field: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "invalid_event_payload")
        .with_context("subscriber", NAME)
        .with_context("event_type", event.event_type.clone()) // clone: the context owns it
        .with_context("field", field)
        .with_context("seq", event.seq)
}

fn positive(event: &SourceEvent, field: &'static str) -> Result<i64, ForgeError> {
    event
        .payload
        .get(field)
        .and_then(serde_json::Value::as_i64)
        .filter(|id| *id > 0)
        .ok_or_else(|| bad_payload(event, field))
}

/// Who did it, for the events that tell. `None`: this event tells nobody
/// (a thread resolved or reopened).
fn actor_of(
    event: &SourceEvent,
    kind: NotificationKind,
) -> Result<Option<Option<UserId>>, ForgeError> {
    Ok(match kind {
        NotificationKind::MergeRequestApproved => {
            Some(Some(UserId::new(positive(event, "approver_id")?)))
        }
        NotificationKind::MergeRequestMerged => {
            Some(Some(UserId::new(positive(event, "merged_by")?)))
        }
        NotificationKind::MergeRequestCommented => {
            let action = event
                .payload
                .get("action")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| bad_payload(event, "action"))?;
            // A new note tells; resolving or reopening a thread does not.
            (action == forge_core::CommentAction::NoteAdded.as_str())
                .then(|| event.actor_user_id.map(UserId::new))
        }
        // M3 X6: the conflict scan (actor system) acted, nobody in person;
        // the event's own merge request's author is told.
        NotificationKind::MergeRequestConflictPredicted => Some(None),
    })
}

impl Notify {
    async fn on_event(
        &self,
        event: &SourceEvent,
        tx: &mut DerivedTx<'_>,
        kind: NotificationKind,
    ) -> Result<(), ForgeError> {
        let project = event
            .project_id
            .ok_or_else(|| bad_payload(event, "project_id"))?;
        let iid = positive(event, "iid")?;
        let Some(actor) = actor_of(event, kind)? else {
            return Ok(());
        };
        let Some(author) = notify::merge_request_author(tx, project, iid).await? else {
            // Purged between the event and this step: nobody to tell.
            tracing::debug!(
                target: "forge_notify",
                event = "forge.notify.merge_request_gone",
                request_id = %event.request_id,
                source_seq = event.seq,
                "the merge request of an event no longer exists"
            );
            return Ok(());
        };
        if actor == Some(author) {
            // Nobody needs to be told about what they did themselves.
            return Ok(());
        }
        if !self.paused {
            let initial = match decide(&self.db, author, kind, project, iid).await? {
                Decision::Send { .. } => Initial::Queue,
                Decision::Skip(reason) => Initial::Skip(reason),
                Decision::Suppress(reason) => Initial::Suppress(reason),
            };
            let new = NewNotification {
                user: author,
                kind,
                project,
                iid,
                actor,
                initial,
            };
            notify::enqueue(tx, event, &new, &self.delivery).await?;
        }
        // M3 U4: the in-app item for the same recipient, mail on or off.
        if decide_in_app(&self.db, author, kind, project, iid).await? {
            let item = in_app::NewItem {
                user: author,
                kind,
                project,
                iid,
                actor,
            };
            in_app::record(tx, event, &item).await?;
        }
        Ok(())
    }
}

impl Subscriber for Notify {
    fn name(&self) -> &'static str {
        NAME
    }

    fn ordering(&self) -> Ordering {
        Ordering::Independent
    }

    fn handle<'a>(&'a self, event: &'a SourceEvent, tx: &'a mut DerivedTx<'_>) -> HandleFuture<'a> {
        Box::pin(async move {
            let Some(event_type) = EventType::parse(&event.event_type) else {
                // A type this build does not know (a newer writer). The
                // subscriber is independent, so it goes on, never silently.
                tracing::warn!(
                    target: "forge_notify",
                    event = "forge.subscribers.unknown_event_type",
                    subscriber = NAME,
                    request_id = %event.request_id,
                    source_seq = event.seq,
                    event_type = %event.event_type,
                    "an event type this build does not know was passed over"
                );
                return Ok(());
            };
            match NotificationKind::from_event(event_type) {
                Some(kind) => self.on_event(event, tx, kind).await,
                None => Ok(()),
            }
        })
    }
}
