//! What a namespace move records (D5, ADR-0015 Amendment D5): the summary
//! in the audit details of `project.moved`, `group.moved` and
//! `user.renamed`, and the inherited roles the move changed.

use crate::ids::{NamespaceId, UserId};
use crate::role::Role;

/// How many role changes the audit details list (the audit row holds
/// 8 KiB; ids of the same transition are grouped, about 12 bytes each).
/// More are counted, never silently dropped (`role_changes_truncated`).
pub const SHOWN_ROLE_CHANGES: usize = 400;

/// One user whose inherited role (from the ancestors of the moved
/// namespace) differs after the move: `None` = no inherited role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleChange {
    pub user: UserId,
    pub from: Option<Role>,
    pub to: Option<Role>,
}

/// A subtree move as its audit row records it: ids, counts and the role
/// changes — never a path or a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveSummary {
    /// The namespace that moved (the root of the subtree).
    pub namespace_id: NamespaceId,
    /// Its parent before and after (`None` = top level).
    pub from_parent: Option<NamespaceId>,
    pub to_parent: Option<NamespaceId>,
    /// Its last segment changed (a rename; with an unchanged parent, a
    /// rename only).
    pub renamed: bool,
    /// Namespaces whose path changed (the root and its descendants).
    pub nodes: i64,
    /// Projects among them (each has its own `project.moved` audit row).
    pub projects: i64,
    /// Tombstones of the same namespaces released to reclaim their paths.
    pub reclaimed_tombstones: i64,
    /// Users whose inherited role changed, by user id (at most 1000 are
    /// read).
    pub role_changes: Vec<RoleChange>,
    /// More than the role changes read exist.
    pub role_changes_truncated: bool,
}

fn role_name(role: Option<Role>) -> &'static str {
    role.map_or("none", Role::as_str)
}

impl MoveSummary {
    /// `{"namespace_id","from_parent_id","to_parent_id","renamed","nodes",
    /// "projects","reclaimed_tombstones","role_changes":{"<from>><to>":[user
    /// ids]},"role_changes_count","role_changes_truncated"}`: the first
    /// [`SHOWN_ROLE_CHANGES`] changes grouped by transition (`none` = no
    /// role), `role_changes_count` all that were read, `truncated` when
    /// some are not listed.
    pub(crate) fn to_value(&self) -> serde_json::Value {
        let mut groups = serde_json::Map::new();
        for change in self.role_changes.iter().take(SHOWN_ROLE_CHANGES) {
            let key = format!("{}>{}", role_name(change.from), role_name(change.to));
            let entry = groups
                .entry(key)
                .or_insert_with(|| serde_json::Value::Array(Vec::new()));
            if let serde_json::Value::Array(ids) = entry {
                ids.push(change.user.get().into());
            }
        }
        serde_json::json!({
            "namespace_id": self.namespace_id.get(),
            "from_parent_id": self.from_parent.map(NamespaceId::get),
            "to_parent_id": self.to_parent.map(NamespaceId::get),
            "renamed": self.renamed,
            "nodes": self.nodes,
            "projects": self.projects,
            "reclaimed_tombstones": self.reclaimed_tombstones,
            "role_changes": groups,
            "role_changes_count": self.role_changes.len(),
            "role_changes_truncated": self.role_changes_truncated
                || self.role_changes.len() > SHOWN_ROLE_CHANGES,
        })
    }
}

/// The kind of a namespace tombstone (ADR-0015).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TombstoneKind {
    /// Redirects to the namespace it names.
    Moved,
    /// Does not redirect (also a `moved` tombstone whose target is gone).
    Deleted,
}

impl TombstoneKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Moved => "moved",
            Self::Deleted => "deleted",
        }
    }
}
