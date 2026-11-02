//! The administration check of `forge doctor` (D3 MR2, ADR-0019 §10,
//! `FG-AUTH-0010`, runbook user-offboarding).

use forge_db::{projects, users, Db};

use super::{fail, ok, skip, warn, Check};

const NAME: &str = "users.administration";

/// `users.administration`: an active human administrator exists, every
/// top-level group has a direct active Owner, and every project (not pending
/// deletion) has an active Owner, direct or inherited (D4 MR4,
/// `reason=ownerless_project`). A forge without any user (a
/// new install) is `warn` without a code: nothing was lost, but nobody can
/// administer it until `forge admin bootstrap-admin` runs.
pub(super) async fn administration_check(db: &Db) -> Check {
    match users::administration(db).await {
        Ok(Some(a)) if a.is_fresh_install() => warn(
            NAME,
            "no users yet: create the first administrator with `forge admin bootstrap-admin`",
        ),
        Ok(Some(a)) => {
            if let Err(e) = a.check() {
                return fail(NAME, e);
            }
            match projects::ownerless(db).await {
                Ok(Some(o)) => match o.check() {
                    Ok(()) => ok(
                        NAME,
                        &format!(
                            "{} active human administrator(s), every top-level group \
                             and every project has an owner",
                            a.active_human_admins
                        ),
                    ),
                    Err(e) => fail(NAME, e),
                },
                Ok(None) => skip(NAME, "schema not migrated"),
                Err(e) => fail(NAME, e),
            }
        }
        Ok(None) => skip(NAME, "schema not migrated"),
        Err(e) => fail(NAME, e),
    }
}
