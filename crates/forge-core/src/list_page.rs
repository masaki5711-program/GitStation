//! Keyset page scans for the API lists (M1 phase 8b MR7b, ADR-0010 /
//! ADR-0015 "一覧"): `GET /api/v1/projects` and `GET /api/v1/groups`.
//!
//! **Rows are filtered by `authorize()`, never by the store** (plan option
//! L1): a loader returns *every* row in id order after a position, with the
//! viewer's raw authorization inputs, and forge-web decides each row with
//! `authorize()` before it shows anything of it. So the store never encodes
//! a visibility rule of its own, and the list cannot drift from the
//! single-object routes.
//!
//! forge-db implements these traits and `forge serve` injects them (the
//! same shape as [`crate::ProjectAccessLoader`]); forge-web never depends on
//! forge-db (CLAUDE.md §3-9).

use crate::namespace_resolver::LookupFuture;
use crate::{GroupId, NamespaceId, NamespacePath, ProjectId, ProjectRecord, Role, UserId};

/// One group as one scanned row: the viewer's authorization input (the
/// effective role) and what the list shows once `authorize()` allowed. All
/// from the same row, so they cannot belong to different groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupListing {
    pub id: GroupId,
    /// The viewer's native effective role (direct or through an ancestor
    /// group); `None` = no role.
    pub viewer_role: Option<Role>,
    /// The group's `forge.namespaces` row.
    pub namespace_id: NamespaceId,
    /// The group's current path, confirmed against its segments.
    pub path: NamespacePath,
    pub display_name: String,
    pub description: String,
    /// `groups.created_at` in UTC, RFC 3339.
    pub created_at: String,
}

/// Projects in id order, for `GET /api/v1/projects`.
pub trait ProjectPageLoader: Send + Sync {
    /// The projects with an id greater than `after` (`None` = from the
    /// start), in ascending id order, at most `max_rows` of them, **whatever
    /// the viewer may see** (deleted ones included): visibility is decided
    /// per row by `authorize()` in the caller.
    ///
    /// **Contract**: one statement per call, the same statement whatever
    /// the rows are and whoever the viewer is (forge-db: one statement).
    /// Errors are store failures (`FG-DB-…`) or rows that cannot be read
    /// (`FG-DB-0021`). The caller bounds the call with its own timeout.
    fn project_page<'a>(
        &'a self,
        after: Option<ProjectId>,
        viewer: UserId,
        max_rows: u32,
    ) -> LookupFuture<'a, Vec<ProjectRecord>>;
}

/// Groups in id order, for `GET /api/v1/groups`.
pub trait GroupPageLoader: Send + Sync {
    /// As [`ProjectPageLoader::project_page`], for groups.
    fn group_page<'a>(
        &'a self,
        after: Option<GroupId>,
        viewer: UserId,
        max_rows: u32,
    ) -> LookupFuture<'a, Vec<GroupListing>>;
}
