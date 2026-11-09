//! A person's in-app notifications (M3 U4, ADR-0016 Amendment U4): what the
//! routes `GET /api/v1/user/notifications`, `…/unread-count`,
//! `POST …/{id}/read` and `…/read-all` read and change.
//!
//! The items were recorded by the `notify` subscriber for the person who
//! could see the merge request then; access changes, so every read decides
//! again — `forge_auth::prove` on `MergeRequestRead` for the person as they
//! are now ([`crate::access::visible_view`], the decision of the web UI).
//! An item that fails it (removed from the project, the project made
//! private or deleted, the merge request purged) is not listed and not
//! counted; nothing about it is returned, not even that it exists. The
//! proof is `NotificationInboxRead` / `…Update` on the caller's own
//! notifications (`authorize()` allows nobody else, administrators
//! included) and every statement is scoped to its owner.

use std::collections::hash_map::Entry;
use std::collections::HashMap;

use forge_auth::{Action, Authorized, NotificationPreferencesCtx, Principal};
use forge_core::ids::{ProjectId, UserId};
use forge_core::ProjectRecord;
use forge_db::notify::in_app::{self, COUNT_CAP};
pub use forge_db::notify::in_app::{Item, PageQuery, MAX_PAGE};
use forge_db::notify::{self};
use forge_db::{resolve, Db};
use forge_errors::{ErrorCode, ForgeError};

use crate::access::{person, visible_view, View};

/// One item the caller may see, with the words to show it by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub item: Item,
    pub view: View,
}

/// A page of the caller's items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub items: Vec<Listed>,
    /// Pass as `before` for the next page; `None`: no more.
    pub next_before: Option<i64>,
}

/// The unread count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnreadCount {
    /// Unread items the caller may see, at most [`COUNT_CAP`]. Counted
    /// over at most [`COUNT_SCAN_PAGES`] pages of unread rows (newest
    /// first): past that, older visible items are not counted.
    pub count: i64,
    /// [`COUNT_CAP`] visible unread items were found (show "99+"). Only
    /// visible items decide it, so a hidden item gives no hint.
    pub capped: bool,
}

/// The most pages of [`COUNT_CAP`] unread rows the count reads.
pub const COUNT_SCAN_PAGES: usize = 10;

/// The in-app notification commands.
#[derive(Clone)]
pub struct Inbox {
    db: Db,
}

/// `FG-SYS-0001 reason=proof_action_mismatch` unless the proof is for
/// `expected`.
fn require(action: Action, expected: Action) -> Result<(), ForgeError> {
    if action == expected {
        return Ok(());
    }
    Err(ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "proof_action_mismatch")
        .with_context("command_action", expected.name())
        .with_context("proof_action", action.name()))
}

fn owner(proof: &Authorized<NotificationPreferencesCtx>) -> UserId {
    *proof.id()
}

impl Inbox {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// The caller as the person the items are decided for (`None`: the
    /// account is gone, so nothing is visible).
    async fn viewer(&self, user: UserId) -> Result<Option<Principal>, ForgeError> {
        Ok(notify::recipient(&self.db, user)
            .await?
            .map(|recipient| person(user, &recipient)))
    }

    /// `items` that `user` may see now, in their order.
    async fn visible(&self, user: UserId, items: Vec<Item>) -> Result<Vec<Listed>, ForgeError> {
        let Some(principal) = self.viewer(user).await? else {
            return Ok(Vec::new());
        };
        let keys: Vec<(ProjectId, i64)> = items.iter().map(|i| (i.project, i.iid)).collect();
        let merge_requests = in_app::merge_requests(&self.db, &keys).await?;
        // One access read per project of the page (usually one or two).
        let mut projects: HashMap<ProjectId, Option<ProjectRecord>> = HashMap::new();
        let mut listed = Vec::with_capacity(items.len());
        for item in items {
            let Some(stored) = merge_requests.get(&(item.project, item.iid)) else {
                continue; // the merge request is gone
            };
            if let Entry::Vacant(slot) = projects.entry(item.project) {
                slot.insert(resolve::load_project(&self.db, item.project, Some(user)).await?);
            }
            let Some(Some(record)) = projects.get(&item.project) else {
                continue; // the project is gone or not visible
            };
            if let Some(view) = visible_view(&principal, record, stored) {
                listed.push(Listed { item, view });
            }
        }
        Ok(listed)
    }

    /// The owner of item `id` (`None`: no such item): the route's loader
    /// reads it before the decision, like a token's owner, so another
    /// person's item and an absent one answer alike.
    pub async fn owner(&self, id: i64) -> Result<Option<UserId>, ForgeError> {
        in_app::owner(&self.db, id).await
    }

    /// `GET /api/v1/user/notifications` (`NotificationInboxRead`): newest
    /// first. A page may hold fewer than `limit` items (the hidden ones are
    /// skipped); `next_before` is set whenever more rows may follow.
    pub async fn list(
        &self,
        proof: &Authorized<NotificationPreferencesCtx>,
        query: PageQuery,
    ) -> Result<Page, ForgeError> {
        require(proof.action(), Action::NotificationInboxRead)?;
        let user = owner(proof);
        let rows = in_app::page(&self.db, user, query).await?;
        let full = i64::try_from(rows.len()).unwrap_or(i64::MAX) >= query.limit;
        let next_before = full.then(|| rows.last().map(|i| i.id)).flatten();
        Ok(Page {
            items: self.visible(user, rows).await?,
            next_before,
        })
    }

    /// `GET …/notifications/unread-count` (`NotificationInboxRead`).
    pub async fn unread_count(
        &self,
        proof: &Authorized<NotificationPreferencesCtx>,
    ) -> Result<UnreadCount, ForgeError> {
        require(proof.action(), Action::NotificationInboxRead)?;
        let user = owner(proof);
        let mut count: i64 = 0;
        let mut before = None;
        // Bounded: at most COUNT_SCAN_PAGES pages, stop at COUNT_CAP visible.
        for _ in 0..COUNT_SCAN_PAGES {
            let rows = in_app::page(
                &self.db,
                user,
                PageQuery {
                    before,
                    unread_only: true,
                    limit: COUNT_CAP,
                },
            )
            .await?;
            let full = i64::try_from(rows.len()).unwrap_or(i64::MAX) >= COUNT_CAP;
            before = rows.last().map(|i| i.id);
            let seen = self.visible(user, rows).await?.len();
            count = count.saturating_add(i64::try_from(seen).unwrap_or(i64::MAX));
            if count >= COUNT_CAP {
                return Ok(UnreadCount {
                    count: COUNT_CAP,
                    capped: true,
                });
            }
            if !full {
                break;
            }
        }
        Ok(UnreadCount {
            count,
            capped: false,
        })
    }

    /// `POST …/notifications/{id}/read` (`NotificationInboxUpdate` on the
    /// item's owner, decided by the route). Marking an item read again
    /// changes nothing; an item of somebody else is `FG-REPO-0005`
    /// not found (the route's decision already refused it; this is the
    /// statement's own scope).
    pub async fn mark_read(
        &self,
        proof: &Authorized<NotificationPreferencesCtx>,
        id: i64,
    ) -> Result<Item, ForgeError> {
        require(proof.action(), Action::NotificationInboxUpdate)?;
        in_app::mark_read(&self.db, owner(proof), id)
            .await?
            .ok_or_else(|| {
                ForgeError::new(ErrorCode::NotFound)
                    .with_context("reason", "in_app_notification_not_found")
            })
    }

    /// `POST …/notifications/read-all` (`NotificationInboxUpdate`): every
    /// unread item of the caller up to `up_to`, hidden ones included. The
    /// number of rows is not returned to the caller (it would count items
    /// the caller may no longer see); it is for the log only.
    pub async fn mark_all_read(
        &self,
        proof: &Authorized<NotificationPreferencesCtx>,
        up_to: Option<i64>,
    ) -> Result<u64, ForgeError> {
        require(proof.action(), Action::NotificationInboxUpdate)?;
        in_app::mark_all_read(&self.db, owner(proof), up_to).await
    }
}
