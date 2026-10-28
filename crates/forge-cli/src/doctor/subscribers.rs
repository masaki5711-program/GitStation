//! The event subscriber checks of `forge doctor` (ADR-0016 §7, step C5,
//! `FG-DB-0008`).

use forge_db::{subscribers, Db};

use super::{fail, ok, skip, Check};

/// `subscribers.lag` (stuck / behind) and `subscribers.dead_letters`
/// (quarantined in the last 24 h).
pub(super) async fn subscriber_checks(db: &Db) -> [Check; 2] {
    let lag = match subscribers::states(db).await {
        Ok(Some(states)) => match subscribers::check_lag(&states) {
            Ok(()) => {
                let behind = states.iter().filter(|s| s.lag_events > 0).count();
                let max = states.iter().map(|s| s.lag_events).max().unwrap_or(0);
                ok(
                    "subscribers.lag",
                    &format!(
                        "{} subscribers, {behind} with pending events (max {max})",
                        states.len()
                    ),
                )
            }
            Err(e) => fail("subscribers.lag", e),
        },
        Ok(None) => skip("subscribers.lag", "schema not migrated"),
        Err(e) => fail("subscribers.lag", e),
    };
    let letters = match subscribers::dead_letters(db).await {
        Ok(Some(d)) => match d.check() {
            Ok(()) => ok(
                "subscribers.dead_letters",
                &format!("{} quarantined, none in the last 24 h", d.total),
            ),
            Err(e) => fail("subscribers.dead_letters", e),
        },
        Ok(None) => skip("subscribers.dead_letters", "schema not migrated"),
        Err(e) => fail("subscribers.dead_letters", e),
    };
    [lag, letters]
}
