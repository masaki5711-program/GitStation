//! The service account checks of `forge doctor` (M3 S0, plan 11 §5 S0,
//! runbook service-accounts): `service_accounts.tokens_expiring`
//! (`FG-AUTH-0019`, warn) and `service_accounts.ownerless` (`FG-AUTH-0010
//! reason=ownerless_service_account`, warn). Read pool, one statement.

use forge_db::service_accounts::{self, EXPIRY_WARNING};
use forge_db::Db;

use super::{fail, ok, skip, Check};

const EXPIRING: &str = "service_accounts.tokens_expiring";
const OWNERLESS: &str = "service_accounts.ownerless";

/// `service_accounts.tokens_expiring`: no active service account whose
/// active tokens all expire within 14 days (rotate before the 365-day
/// limit). `service_accounts.ownerless`: every active service account has
/// an active person as its owner.
pub(super) async fn service_account_checks(db: &Db) -> [Check; 2] {
    match service_accounts::health(db, EXPIRY_WARNING).await {
        Ok(Some(h)) => {
            let expiring = match h.check_expiring() {
                Ok(()) => ok(
                    EXPIRING,
                    &format!(
                        "no service account loses its last token within {} days",
                        EXPIRY_WARNING.as_secs() / 86_400
                    ),
                ),
                Err(e) => fail(EXPIRING, e),
            };
            let ownerless = match h.check_ownerless() {
                Ok(()) => ok(
                    OWNERLESS,
                    "every active service account has an active owner",
                ),
                Err(e) => fail(OWNERLESS, e),
            };
            [expiring, ownerless]
        }
        Ok(None) => [
            skip(EXPIRING, "schema not migrated"),
            skip(OWNERLESS, "schema not migrated"),
        ],
        Err(e) => [
            fail(EXPIRING, e),
            skip(OWNERLESS, "service accounts unavailable"),
        ],
    }
}
