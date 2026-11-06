//! Who may be told about what, decided twice with the same function: when
//! the `notify` subscriber records a notification, and again just before
//! the mail is built (people lose access, switch mail off or are disabled
//! between the two).
//!
//! Visibility is `forge_auth::prove` on `MergeRequestRead` with the
//! recipient as a signed-in person — the same decision that shows or hides
//! the merge request in the web UI; this crate has no access rule of its
//! own. A project the recipient cannot see, a project in its deletion grace
//! period and one that no longer exists all read as "not visible".

use forge_auth::{
    prove, Action, Credential, LoadedMergeRequest, LoadedProject, MergeRequestCtx, Principal,
};
use forge_core::config::valid_mailbox;
use forge_core::ids::{ProjectId, UserId};
use forge_core::{NotificationKind, UserKind};
use forge_db::notify::{self, Preferences, Recipient};
use forge_db::{resolve, review, Db};
use forge_errors::ForgeError;

/// Reasons a notification is not sent (`forge.notifications.reason`).
pub mod reason {
    pub const RECIPIENT_GONE: &str = "recipient_not_found";
    pub const RECIPIENT_DISABLED: &str = "recipient_inactive";
    pub const NOT_A_PERSON: &str = "recipient_not_person";
    pub const NO_ADDRESS: &str = "no_address";
    pub const ADDRESS_INVALID: &str = "address_invalid";
    pub const NOT_VISIBLE: &str = "not_visible";
    pub const MERGE_REQUEST_GONE: &str = "merge_request_gone";
    pub const MUTED: &str = "muted";
    pub const OPTED_OUT: &str = "opted_out";
}

/// What a recipient may be sent: the words the mail is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub project_path: String,
    pub title: String,
}

/// The decision about one notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Send it, with this person's settings and what they may see.
    Send {
        recipient: Recipient,
        preferences: Preferences,
        view: View,
    },
    /// The person chose not to receive it.
    Skip(&'static str),
    /// It cannot or must not be sent.
    Suppress(&'static str),
}

/// Decide whether `user` is to be sent a `kind` notification about merge
/// request `iid` of `project` now.
pub async fn decide(
    db: &Db,
    user: UserId,
    kind: NotificationKind,
    project: ProjectId,
    iid: i64,
) -> Result<Decision, ForgeError> {
    let Some(recipient) = notify::recipient(db, user).await? else {
        return Ok(Decision::Suppress(reason::RECIPIENT_GONE));
    };
    if !recipient.active {
        return Ok(Decision::Suppress(reason::RECIPIENT_DISABLED));
    }
    // Only people are told: a service account or a bot has no inbox.
    if recipient.kind != UserKind::Human {
        return Ok(Decision::Suppress(reason::NOT_A_PERSON));
    }
    match recipient.email.as_deref() {
        None => return Ok(Decision::Suppress(reason::NO_ADDRESS)),
        Some(email) if !valid_mailbox(email) => {
            return Ok(Decision::Suppress(reason::ADDRESS_INVALID));
        }
        Some(_) => {}
    }
    let preferences = notify::preferences(db, user).await?;
    if preferences.muted.contains(&kind) {
        return Ok(Decision::Skip(reason::MUTED));
    }
    if !preferences.email_enabled {
        return Ok(Decision::Skip(reason::OPTED_OUT));
    }
    match merge_request_view(db, user, &recipient, project, iid).await? {
        Seen::Visible(view) => Ok(Decision::Send {
            recipient,
            preferences,
            view,
        }),
        Seen::NotVisible => Ok(Decision::Suppress(reason::NOT_VISIBLE)),
        Seen::Gone => Ok(Decision::Suppress(reason::MERGE_REQUEST_GONE)),
    }
}

enum Seen {
    Visible(View),
    NotVisible,
    Gone,
}

/// `MergeRequestRead` for `user` as a person in a session, on the project
/// and merge request as they are now.
async fn merge_request_view(
    db: &Db,
    user: UserId,
    recipient: &Recipient,
    project: ProjectId,
    iid: i64,
) -> Result<Seen, ForgeError> {
    let Some(record) = resolve::load_project(db, project, Some(user)).await? else {
        return Ok(Seen::NotVisible);
    };
    let Some(stored) = review::get(db, project, iid).await? else {
        // The project is there but the merge request is not (its project
        // access is decided below only for one that exists).
        return Ok(Seen::Gone);
    };
    let principal = Principal::authenticated(
        user,
        recipient.is_admin,
        recipient.active,
        Credential::Session,
        recipient.kind,
    );
    let loaded = LoadedProject {
        id: record.id,
        visibility: record.access.visibility,
        role: record.access.role,
        pending_deletion: record.access.pending_deletion,
    };
    let mr = LoadedMergeRequest {
        iid: Some(stored.iid),
        author: stored.author,
        state: stored.state,
        draft: stored.draft,
        source_branch: stored.source_branch.clone(), // clone: the context owns its names
        target_branch: stored.target_branch.clone(),
    };
    let ctx = MergeRequestCtx::new(loaded, mr);
    if prove(&principal, Action::MergeRequestRead, ctx).is_err() {
        return Ok(Seen::NotVisible);
    }
    Ok(Seen::Visible(View {
        project_path: record.path.full_path(),
        title: stored.title,
    }))
}
