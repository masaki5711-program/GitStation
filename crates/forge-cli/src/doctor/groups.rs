//! The group visibility check of `forge doctor` (plan 15 MR 3, §4.1;
//! ADR-0010 Amendment "グループの可視性"): `groups.visibility`, info only.

use forge_db::{groups, Db};

use super::{fail, ok, skip, Check};

pub(super) const NAME: &str = "groups.visibility";

/// `groups.visibility`: how many groups and live projects are stored
/// `internal` below a private group. Legal and private in effect (the
/// narrowest on the chain wins; a narrowing never rewrites the rows below),
/// so never a failure — but widening the group above would expose them all
/// at once, which the Owner should know. No FG code, no runbook.
pub(super) async fn visibility_check(db: &Db) -> Check {
    match groups::wider_than_parent(db).await {
        Ok(Some(w)) if w.groups == 0 && w.projects == 0 => ok(
            NAME,
            "no group or project is stored wider than the group above it",
        ),
        Ok(Some(w)) => ok(
            NAME,
            &format!(
                "info: {} group(s) and {} project(s) are stored internal below a private \
                 group (private in effect; widening the group above exposes them)",
                w.groups, w.projects
            ),
        ),
        Ok(None) => skip(NAME, "schema not migrated"),
        Err(e) => fail(NAME, e),
    }
}
