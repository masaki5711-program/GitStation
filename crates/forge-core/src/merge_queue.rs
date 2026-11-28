//! The merge queue's vocabulary (M4 C8, ADR-0030): the states of an entry
//! and of a batch, why an entry runs alone, the batch bound and the
//! bisection rule, and the audit details of a person's change to a queue.
//! Pure: the rows are forge-db's (`forge_db::review::merge_queue`), the
//! git work and every decision forge-review's (`forge_review::merges::queue`).

use crate::{MergeRequestId, ProjectId};

/// The most entries one batch can hold: the schema's bound (`CHECK 1..64`
/// on `merge_queues.batch_limit` and `merge_queue_batches.size`, migration
/// `merge_queue_batch_limit_64`, plan 18 S7).
pub const MAX_BATCH: usize = 64;

/// The stored `batch_limit` that means "not lowered" (plan 18 S7): 4, the
/// full bound of the binaries before S7, so a rollback to one of them reads
/// every stored bound (they refuse a row above 4). A queue stores this
/// after a landing and by default; a bisection or a cut stores a lowered
/// bound below it ([`stored_lowered`]). The bound a batch is built with is
/// [`batch_bound`].
pub const FULL_MARK: usize = 4;

/// The stored form of a lowered bound `limit` (a bisection, a merge
/// result's cut): `1..FULL_MARK`. A cut deeper than that in a document
/// batch is stored as `FULL_MARK - 1` — the next batch is smaller than
/// needed, never larger, and the landing resets it.
pub fn stored_lowered(limit: usize) -> usize {
    limit.clamp(1, FULL_MARK - 1)
}

/// `merge_queue.doc_batch_max`'s default: a queue whose verdict is in
/// process — no `.forge-ci.yml` at its base or its train (plan 18 S7,
/// ADR-0037 (b) 5).
pub const DEFAULT_DOC_BATCH: usize = 32;

/// `merge_queue.ci_batch_max`'s default: a queue whose train has a
/// pipeline (plan 14 §9 decision 5, ADR-0030).
pub const DEFAULT_CI_BATCH: usize = 4;

/// The bound of the next batch: its kind's bound — `doc_max` when the
/// verdict is in process (`in_process`: no CI definition), `ci_max`
/// otherwise — lowered by the queue's stored `batch_limit` unless that is
/// [`FULL_MARK`] ("not lowered"); always within `1..=`[`MAX_BATCH`].
pub fn batch_bound(stored: usize, in_process: bool, doc_max: usize, ci_max: usize) -> usize {
    let kind = if in_process { doc_max } else { ci_max };
    let lowered = if stored >= FULL_MARK {
        MAX_BATCH
    } else {
        stored
    };
    lowered.min(kind).clamp(1, MAX_BATCH)
}

/// The most live batches one queue may hold at once (plan 18 S9,
/// `merge_queue.speculation_depth`'s upper bound): the schema's CHECK
/// `merge_queue_batches_depth` (`depth BETWEEN 1 AND 8`, migration
/// `merge_queue_speculation`) holds the same number: change both together.
pub const MAX_SPECULATION: usize = 8;

/// `merge_queue.speculation_depth`'s default: one live batch per queue,
/// ADR-0030's queue (plan 18 S9). Higher only where the runners can test
/// that many trains at once (plan 18 §5 decision 2 is still open).
pub const DEFAULT_SPECULATION: usize = 1;

/// Whether a queue with `live` live batches builds one more behind them
/// (plan 18 S9): only while its front waits for its train's pipeline
/// (`front_waiting`), the queue is not paused, and fewer than `depth`
/// (clamped to `1..=`[`MAX_SPECULATION`]) are live. Depth 1 never builds
/// behind a live batch.
pub fn builds_behind(live: usize, depth: usize, front_waiting: bool, paused: bool) -> bool {
    front_waiting && !paused && live >= 1 && live < depth.clamp(1, MAX_SPECULATION)
}

/// An entry reaped this many times (its batch passed the deadline) is
/// removed (`FG-CI-0024`, ADR-0030 §14). The table's CHECK
/// `merge_queue_entries_reaps` (`reaps BETWEEN 0 AND 3`, migration
/// 20261213100000) holds the same number: change both together.
pub const MAX_REAPS: i32 = 3;

/// The batch bound after a red batch of `size` entries (ADR-0030 §7): the
/// first half (rounded up) is tried next; a red batch of one is the
/// culprit's, the bound stays.
pub fn bisected_limit(size: usize) -> usize {
    if size <= 1 {
        1
    } else {
        size.div_ceil(2)
    }
}

macro_rules! words {
    ($(#[$doc:meta])* $name:ident { $($(#[$vdoc:meta])* $variant:ident => $word:literal),+ $(,)? }) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vdoc])* $variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $word),+
                }
            }

            pub fn parse(s: &str) -> Option<Self> {
                match s {
                    $($word => Some($name::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

words! {
    /// `merge_queue_entries.state`.
    EntryState {
        /// Waiting for a batch.
        Queued => "queued",
        /// In the live batch.
        Testing => "testing",
        /// Landed: its merge attempt is `applied`.
        Merged => "merged",
        /// Out of the queue (a person, or the worker with a reason and code).
        Removed => "removed",
    }
}

impl EntryState {
    /// `queued` or `testing`: at most one per merge request.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Queued | Self::Testing)
    }
}

words! {
    /// `merge_queue_batches.state`.
    BatchState {
        /// Its train pipeline runs (or is about to).
        Testing => "testing",
        /// Green: the merge attempts are being written and the target moved.
        Landing => "landing",
        /// The target is at the train; every entry `merged`.
        Landed => "landed",
        /// Red: bisected (2+) or its one entry removed.
        Failed => "failed",
        /// Thrown away without a verdict (target moved, an entry left,
        /// timeout, a refusal at landing): the entries went back.
        Abandoned => "abandoned",
    }
}

impl BatchState {
    /// `testing` or `landing`: at most one per (queue, depth) — plan 18
    /// S9; one per queue at `merge_queue.speculation_depth = 1`.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Testing | Self::Landing)
    }
}

words! {
    /// Why an entry runs in a batch of its own (ADR-0030 §4).
    Isolation {
        /// Its diff touches a migration (`risk::is_migration_path`).
        Migration => "migration",
        /// The head's `forge/risk` run concluded `failure` (a `stop`).
        RiskStop => "risk_stop",
        /// Its method is `fast_forward` (a train is no fast-forward).
        FastForward => "fast_forward",
    }
}

/// A person's change to a merge queue (audited on the project, no event).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeQueueChange {
    /// `merge_queue.enqueued`.
    Enqueued {
        project: ProjectId,
        merge_request: MergeRequestId,
        iid: i64,
        entry: i64,
        target_branch: String,
        head_sha: String,
    },
    /// `merge_queue.auto_enqueued` (M4 C9): the auto-merge principal added
    /// the merge request (actor `system`).
    AutoEnqueued {
        project: ProjectId,
        merge_request: MergeRequestId,
        iid: i64,
        entry: i64,
        target_branch: String,
        head_sha: String,
    },
    /// `merge_queue.dequeued`.
    Dequeued {
        project: ProjectId,
        merge_request: MergeRequestId,
        iid: i64,
        entry: i64,
        target_branch: String,
    },
    /// `merge_queue.paused`.
    Paused {
        project: ProjectId,
        target_branch: String,
    },
    /// `merge_queue.resumed`.
    Resumed {
        project: ProjectId,
        target_branch: String,
    },
}

impl MergeQueueChange {
    pub fn action(&self) -> &'static str {
        match self {
            Self::Enqueued { .. } => "merge_queue.enqueued",
            Self::AutoEnqueued { .. } => "merge_queue.auto_enqueued",
            Self::Dequeued { .. } => "merge_queue.dequeued",
            Self::Paused { .. } => "merge_queue.paused",
            Self::Resumed { .. } => "merge_queue.resumed",
        }
    }

    pub fn project(&self) -> ProjectId {
        match self {
            Self::Enqueued { project, .. }
            | Self::AutoEnqueued { project, .. }
            | Self::Dequeued { project, .. }
            | Self::Paused { project, .. }
            | Self::Resumed { project, .. } => *project,
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::Enqueued {
                project,
                merge_request,
                iid,
                entry,
                target_branch,
                head_sha,
            } => serde_json::json!({
                "project_id": project.get(),
                "merge_request_id": merge_request.get(),
                "iid": iid,
                "entry_id": entry,
                "target_branch": target_branch,
                "head_sha": head_sha,
            }),
            Self::AutoEnqueued {
                project,
                merge_request,
                iid,
                entry,
                target_branch,
                head_sha,
            } => serde_json::json!({
                "project_id": project.get(),
                "merge_request_id": merge_request.get(),
                "iid": iid,
                "entry_id": entry,
                "target_branch": target_branch,
                "head_sha": head_sha,
                "by": "auto_merge",
            }),
            Self::Dequeued {
                project,
                merge_request,
                iid,
                entry,
                target_branch,
            } => serde_json::json!({
                "project_id": project.get(),
                "merge_request_id": merge_request.get(),
                "iid": iid,
                "entry_id": entry,
                "target_branch": target_branch,
            }),
            Self::Paused {
                project,
                target_branch,
            }
            | Self::Resumed {
                project,
                target_branch,
            } => serde_json::json!({
                "project_id": project.get(),
                "target_branch": target_branch,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_red_batch_halves_rounding_up() {
        assert_eq!(bisected_limit(4), 2);
        assert_eq!(bisected_limit(3), 2);
        assert_eq!(bisected_limit(2), 1);
        assert_eq!(bisected_limit(1), 1);
        assert_eq!(bisected_limit(0), 1);
    }

    #[test]
    fn a_batch_is_bound_by_its_kind_and_by_the_bisection() {
        // A fresh or landed queue (the stored bound is the full mark).
        let (doc, ci) = (DEFAULT_DOC_BATCH, DEFAULT_CI_BATCH);
        assert_eq!(batch_bound(FULL_MARK, true, doc, ci), 32);
        assert_eq!(batch_bound(FULL_MARK, false, doc, ci), 4);
        // A bisected CI queue, a cut document queue.
        assert_eq!(batch_bound(2, false, doc, ci), 2);
        assert_eq!(batch_bound(3, true, doc, ci), 3);
        // A lowered bound is stored below the full mark: a rollback to a
        // binary before S7 reads every stored bound.
        assert_eq!(stored_lowered(2), 2);
        assert_eq!(stored_lowered(4), 3);
        assert_eq!(stored_lowered(31), 3);
        assert_eq!(stored_lowered(0), 1);
        // Never 0, never past the schema.
        assert_eq!(batch_bound(0, true, 0, 0), 1);
        assert_eq!(batch_bound(FULL_MARK, true, 500, 4), MAX_BATCH);
    }

    #[test]
    fn only_a_waiting_front_has_trains_built_behind_it() {
        // Depth 1 (the default): never a second live batch.
        assert_eq!(DEFAULT_SPECULATION, 1);
        assert!(!builds_behind(1, DEFAULT_SPECULATION, true, false));
        // Depth 3: up to three, behind a waiting, unpaused front.
        assert!(builds_behind(1, 3, true, false));
        assert!(builds_behind(2, 3, true, false));
        assert!(!builds_behind(3, 3, true, false));
        assert!(!builds_behind(1, 3, false, false), "a front being decided");
        assert!(!builds_behind(1, 3, true, true), "paused");
        assert!(!builds_behind(0, 3, true, false), "no front: a plain build");
        // Never past the schema.
        assert!(builds_behind(7, 50, true, false));
        assert!(!builds_behind(MAX_SPECULATION, 50, true, false));
        assert!(!builds_behind(1, 0, true, false));
    }

    #[test]
    fn the_words_round_trip() {
        for s in EntryState::ALL {
            assert_eq!(EntryState::parse(s.as_str()), Some(*s));
        }
        for s in BatchState::ALL {
            assert_eq!(BatchState::parse(s.as_str()), Some(*s));
        }
        for s in Isolation::ALL {
            assert_eq!(Isolation::parse(s.as_str()), Some(*s));
        }
        assert!(EntryState::parse("other").is_none());
        assert!(EntryState::Testing.is_live() && !EntryState::Merged.is_live());
        assert!(BatchState::Landing.is_live() && !BatchState::Abandoned.is_live());
    }

    #[test]
    fn the_audit_actions_are_named_by_the_queue() {
        let p = ProjectId::new(1);
        let change = MergeQueueChange::Paused {
            project: p,
            target_branch: "refs/heads/main".to_owned(),
        };
        assert_eq!(change.action(), "merge_queue.paused");
        assert_eq!(change.project(), p);
        assert_eq!(change.to_value()["target_branch"], "refs/heads/main");
    }
}
