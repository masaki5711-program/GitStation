//! Document revisions and edit claims (plan 13 §7.5 and stage 4 part A,
//! ADR-0027): the pure part.
//!
//! - **One open revision per document** (`single_revision`): a document
//!   (`id`, and each translation `<id>.<lang>` on its own) is revised by at
//!   most one open merge request. Which documents a merge request revises
//!   is derived from git (its changed `.md` files whose front matter names
//!   a `doctype`) and stored by forge-db per merge request; nothing is
//!   written into a file.
//! - **The edit claim** ("編集中"): a time-limited, visible, releasable
//!   claim of one person on one document. It lapses [`EDIT_CLAIM_MINUTES`]
//!   after it was taken or last extended (the database's clock), is
//!   released when its holder opens a merge request revising the document,
//!   and may be revoked by a Maintainer (audited here:
//!   [`DocEditClaimChange`]).
//!
//! TODO(plan 13 stage 4 part B): the e-approval additions (reconfirmation
//! before approving, the approval's meaning, binding the approval to the
//! document's blob, ADR-0028) and the merge-time refusal of a second open
//! revision (ADR-0027 §1 "マージのとき") belong to part B, which changes
//! the merge request approvals and `evaluate_merge`. The repository setting
//! `single_revision` / `free` per type (ADR-0027 §2), the owner group's
//! release (§3) and section-level claims are not built yet either: every
//! document is `single_revision`.

use crate::ids::{ProjectId, UserId};

/// An edit claim lapses this many minutes after it was taken or last
/// extended (ADR-0027 §3: "操作がないと 30 分で切れる").
pub const EDIT_CLAIM_MINUTES: i64 = 30;

/// Live claims one person may hold in one project at most (review
/// MEDIUM: a claim is a hint for one document being edited, not a way to
/// lock a repository). One more is `FG-WEB-0006 reason=too_many_claims`.
pub const MAX_LIVE_CLAIMS_PER_PERSON: i64 = 20;

/// A document key as stored: the document's `id` and, for a translation,
/// its language (`None`: the original).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocKey {
    pub doc_id: String,
    pub lang: Option<String>,
}

impl DocKey {
    pub fn original(doc_id: impl Into<String>) -> Self {
        Self {
            doc_id: doc_id.into(),
            lang: None,
        }
    }

    /// `<id>` or `<id>.<lang>` (the file name without `.md`), for logs and
    /// messages.
    pub fn display(&self) -> String {
        match &self.lang {
            Some(l) => format!("{}.{l}", self.doc_id),
            None => self.doc_id.clone(), // clone: the text is owned
        }
    }

    /// The stored language column (`''` for the original).
    pub fn lang_column(&self) -> &str {
        self.lang.as_deref().unwrap_or("")
    }
}

/// A Maintainer's revocation of another person's edit claim
/// (`audit_log.details`, on the project; no event).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocEditClaimChange {
    /// `document.edit_claim.revoked`: the claim of `holder` on `doc_id`
    /// was removed by someone else (the audit row's actor).
    Revoked {
        project: ProjectId,
        doc_id: String,
        holder: UserId,
        merge_request_iid: Option<i64>,
        /// RFC 3339, UTC: when the claim would have lapsed.
        expires_at: String,
    },
}

impl DocEditClaimChange {
    pub fn action(&self) -> &'static str {
        match self {
            Self::Revoked { .. } => "document.edit_claim.revoked",
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::Revoked {
                project,
                doc_id,
                holder,
                merge_request_iid,
                expires_at,
            } => serde_json::json!({
                "project_id": project.get(),
                "doc_id": doc_id,
                "holder_id": holder.get(),
                "merge_request_iid": merge_request_iid,
                "expires_at": expires_at,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_displays_as_its_file_name() {
        assert_eq!(DocKey::original("QP-07-001").display(), "QP-07-001");
        let t = DocKey {
            doc_id: "QP-07-001".into(),
            lang: Some("en".into()),
        };
        assert_eq!(t.display(), "QP-07-001.en");
        assert_eq!(t.lang_column(), "en");
        assert_eq!(DocKey::original("X").lang_column(), "");
    }

    #[test]
    fn the_revocation_names_the_holder_and_the_document() {
        let c = DocEditClaimChange::Revoked {
            project: ProjectId::new(7),
            doc_id: "QP-07-001".into(),
            holder: UserId::new(3),
            merge_request_iid: None,
            expires_at: "2026-11-23T10:00:00.000000Z".into(),
        };
        assert_eq!(c.action(), "document.edit_claim.revoked");
        assert_eq!(
            c.to_value(),
            serde_json::json!({"project_id": 7, "doc_id": "QP-07-001", "holder_id": 3,
                "merge_request_iid": null, "expires_at": "2026-11-23T10:00:00.000000Z"})
        );
    }
}
