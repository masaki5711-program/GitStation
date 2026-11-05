#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

use super::*;

/// The default bounds keep one scan's events under a change's cap (two per
/// tried pair) and its git time under the job's limit.
#[test]
fn the_default_bounds_fit_the_job() {
    let l = ScanLimits::default();
    assert!(l.pairs_max * 2 <= forge_db::change::MAX_EVENTS);
    assert!(l.pairs_max <= l.peers_max);
    assert!(l.budget + Duration::from_secs(30) <= forge_db::review::CONFLICT_SCAN_TIMEOUT);
    assert!(l.merge_tree.timeout <= l.budget);
}
