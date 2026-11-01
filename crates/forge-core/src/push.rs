//! Recording a push (M1 phase 9 MR2, ADR-0008, ADR-0016 §5 / §6 and its
//! phase 9 amendment). git updates the refs outside any database
//! transaction; afterwards forge-web (MR4) logs [`crate::push_log::PushApplied`]
//! and asks a [`PushRecorder`] (forge-db: `repositories::record_push`) to
//! write one `repository.pushed` audit row and one event. forge-web never
//! depends on forge-db (CLAUDE.md §3-9), so the types live here.
//!
//! - [`PushRecord`] is what was pushed: every ref update with its old and
//!   new object, its kind and the new commits git listed (at most
//!   [`MAX_LISTED_COMMITS`] per ref plus the total). [`PushRecord::check`]
//!   refuses an inconsistent record (`FG-DB-0010 reason=invalid_push_record`).
//! - [`PushRecord::digest`] is the idempotency hash of the event's dedup key
//!   `repository_pushed:<repo_id>:<hex sha256(request_id ‖ sorted updates)>`:
//!   the request id tells two pushes with the same updates apart (A→B→A),
//!   the logged request id makes the runbook replay a no-op once recorded.
//! - [`push_json`] is the bounded JSON of the audit details and the event
//!   payload: at most [`MAX_LISTED_REFS`] refs and [`MAX_LISTED_COMMITS`]
//!   commits per ref, within a byte budget, with `total_refs`,
//!   `total_commits`, `refs_truncated` and `commits_truncated` always
//!   stated (never cut silently).
//! - [`PushRecord::failed`] is `FG-GIT-0012` PushRecordFailed: the refs are
//!   updated but the record could not be written (MR4 after its retries).

use std::net::IpAddr;

use forge_errors::{ErrorCode, ForgeError};
use sha2::{Digest, Sha256};

use crate::namespace_resolver::LookupFuture;
use crate::{ProjectId, RefName, RepoId, RequestId, UserId};

/// The most ref updates one push may carry (MR4 refuses more commands
/// before git runs).
pub const MAX_PUSH_UPDATES: usize = 1000;
/// The most refs the audit details and the event list (ADR-0016 §5).
pub const MAX_LISTED_REFS: usize = 100;
/// The most commits listed per ref (ADR-0016 §5); MR4 lists at most these.
pub const MAX_LISTED_COMMITS: usize = 20;
/// The largest `total_commits` of one ref (a bounded `rev-list --count`).
pub const MAX_TOTAL_COMMITS: u64 = 1 << 32;
/// The serialized size the audit details are built within (the stored
/// jsonb may be at most 8 KiB; the rest is headroom for jsonb's own
/// overhead).
pub const AUDIT_BUDGET: usize = 6 * 1024;
/// The serialized size the event payload is built within (stored at most
/// 64 KiB).
pub const EVENT_BUDGET: usize = 48 * 1024;
/// What the top-level fields of [`push_json`] take at most (counted
/// against the budget before any ref).
const HEADER_BYTES: usize = 512;

/// A git object name: 40 (SHA-1) or 64 (SHA-256) lowercase hex digits.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(String);

impl ObjectId {
    /// `FG-DB-0010 reason=invalid_push_record rule=object_id` for anything
    /// else (the value is not quoted).
    pub fn parse(raw: &str) -> Result<Self, ForgeError> {
        let ok = matches!(raw.len(), 40 | 64)
            && raw
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if ok {
            Ok(Self(raw.to_owned()))
        } else {
            Err(invalid("object_id").with_context("len", raw.len()))
        }
    }

    /// The all-zero name of a missing side (create: old, delete: new).
    pub fn is_zero(&self) -> bool {
        self.0.bytes().all(|b| b == b'0')
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How one ref moved (forge decides fast-forward vs force with git before
/// recording).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PushUpdateKind {
    Create,
    FastForward,
    /// Not a fast-forward: history was rewritten.
    Force,
    Delete,
}

impl PushUpdateKind {
    pub const ALL: [PushUpdateKind; 4] = [
        PushUpdateKind::Create,
        PushUpdateKind::FastForward,
        PushUpdateKind::Force,
        PushUpdateKind::Delete,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::FastForward => "fast_forward",
            Self::Force => "force",
            Self::Delete => "delete",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == raw)
    }
}

/// One updated ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushedRef {
    pub name: RefName,
    pub old: ObjectId,
    pub new: ObjectId,
    pub kind: PushUpdateKind,
    /// New commits (newest first), at most [`MAX_LISTED_COMMITS`]; empty
    /// for a delete.
    pub commits: Vec<ObjectId>,
    /// All new commits of this update (`commits` may list fewer).
    pub total_commits: u64,
}

/// One applied push: the repository and every ref git updated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRecord {
    pub repository_id: RepoId,
    pub updates: Vec<PushedRef>,
}

fn invalid(rule: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::ChangeRejected)
        .with_context("reason", "invalid_push_record")
        .with_context("rule", rule)
}

/// `bytes` with a big-endian length prefix (no two inputs share an encoding).
fn framed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

impl PushRecord {
    /// Whether the record describes a push git can have applied:
    /// 1..=[`MAX_PUSH_UPDATES`] updates with distinct ref names, every
    /// object id of one hash length, old ≠ new, the kind matching the zero
    /// sides (create ⇔ old is zero, delete ⇔ new is zero), a delete
    /// without commits, at most [`MAX_LISTED_COMMITS`] listed and no more
    /// than `total_commits` ≤ [`MAX_TOTAL_COMMITS`]. Anything else is
    /// `FG-DB-0010 reason=invalid_push_record` with the broken `rule` (a
    /// bug in the caller; nothing is written).
    pub fn check(&self) -> Result<(), ForgeError> {
        let Some(first) = self.updates.first() else {
            return Err(invalid("no_updates"));
        };
        if self.updates.len() > MAX_PUSH_UPDATES {
            return Err(invalid("too_many_updates").with_context("updates", self.updates.len()));
        }
        let width = first.old.as_str().len();
        let mut names: Vec<&str> = self.updates.iter().map(|u| u.name.as_str()).collect();
        names.sort_unstable();
        if names.windows(2).any(|w| w[0] == w[1]) {
            return Err(invalid("duplicate_ref"));
        }
        for u in &self.updates {
            let mut ids = std::iter::once(&u.old)
                .chain(std::iter::once(&u.new))
                .chain(u.commits.iter());
            if ids.any(|id| id.as_str().len() != width) {
                return Err(invalid("mixed_hash_length"));
            }
            if u.old == u.new {
                return Err(invalid("unchanged_ref"));
            }
            let expected = match (u.old.is_zero(), u.new.is_zero()) {
                (true, true) => return Err(invalid("unchanged_ref")),
                (true, false) => Some(PushUpdateKind::Create),
                (false, true) => Some(PushUpdateKind::Delete),
                (false, false) => None,
            };
            let kind_ok = match expected {
                Some(k) => u.kind == k,
                None => matches!(u.kind, PushUpdateKind::FastForward | PushUpdateKind::Force),
            };
            if !kind_ok {
                return Err(invalid("kind_mismatch").with_context("kind", u.kind.as_str()));
            }
            if u.kind == PushUpdateKind::Delete && (u.total_commits != 0 || !u.commits.is_empty()) {
                return Err(invalid("delete_with_commits"));
            }
            if u.commits.len() > MAX_LISTED_COMMITS {
                return Err(invalid("too_many_commits").with_context("commits", u.commits.len()));
            }
            if u.total_commits > MAX_TOTAL_COMMITS || (u.commits.len() as u64) > u.total_commits {
                return Err(invalid("total_commits"));
            }
        }
        Ok(())
    }

    /// The updates in the order the digest and the JSON use: by ref name.
    pub fn sorted_updates(&self) -> Vec<PushedRef> {
        let mut updates = self.updates.clone(); // clone: the caller keeps its record
        updates.sort_by(|a, b| a.name.cmp(&b.name));
        updates
    }

    /// SHA-256 of a domain tag, `request_id` and each update sorted by ref
    /// name (name, old, new), every field length-prefixed. Kinds and commits
    /// are not part of it: they follow from old / new in the repository.
    pub fn digest(&self, request_id: &RequestId) -> [u8; 32] {
        let mut hasher = Sha256::new();
        framed(&mut hasher, b"forge.repository_pushed.v1");
        framed(&mut hasher, request_id.as_str().as_bytes());
        for u in self.sorted_updates() {
            framed(&mut hasher, u.name.as_str().as_bytes());
            framed(&mut hasher, u.old.as_str().as_bytes());
            framed(&mut hasher, u.new.as_str().as_bytes());
        }
        hasher.finalize().into()
    }

    /// `FG-GIT-0012` PushRecordFailed: the refs of this push are updated but
    /// its record could not be written after `attempts` tries, because of
    /// `cause` (its code goes into the context, the error is the source).
    /// The `forge.git.push_applied` log line of the same request id is the
    /// input of the replay (runbook `push-record-failed`).
    pub fn failed(&self, cause: ForgeError, attempts: u32) -> ForgeError {
        ForgeError::new(ErrorCode::PushRecordFailed)
            .with_context("reason", "record_failed")
            .with_context("repo_id", self.repository_id.get())
            .with_context("refs", self.updates.len())
            .with_context("attempts", attempts)
            .with_context("cause_code", cause.code().id())
            .with_source(cause)
    }
}

/// One ref of the JSON: name, both sides, kind, the listed commits and
/// whether they are all of them.
fn ref_entry(u: &PushedRef, listed: usize) -> serde_json::Value {
    serde_json::json!({
        "ref": u.name.as_str(),
        "old": u.old.as_str(),
        "new": u.new.as_str(),
        "kind": u.kind.as_str(),
        "total_commits": u.total_commits,
        "commits": u.commits.iter().take(listed).map(ObjectId::as_str).collect::<Vec<_>>(),
        "commits_truncated": (listed as u64) < u.total_commits,
    })
}

/// The original pusher of a push recorded by the runbook replay (`forge
/// admin repo record-push`): the replay's actor is the operator, the
/// pusher named by the log line goes into the JSON as `replayed_from`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PushReplay {
    pub pusher: UserId,
    pub credential_id: Option<i64>,
}

/// The listing order of the kinds: the history-rewriting and removing
/// updates first, so they survive truncation.
fn listing_rank(kind: PushUpdateKind) -> u8 {
    match kind {
        PushUpdateKind::Force => 0,
        PushUpdateKind::Delete => 1,
        PushUpdateKind::Create => 2,
        PushUpdateKind::FastForward => 3,
    }
}

/// The JSON of a push, built within `budget` serialized bytes:
/// `{"project_id","repository_id","total_refs","refs_truncated",
/// "created","fast_forward","forced","deleted","total_commits",
/// "commits_truncated","replayed_from","refs":[{"ref","old","new","kind",
/// "total_commits","commits","commits_truncated"}]}`. The four kind counts
/// cover every update (listed or not); `replayed_from` is
/// `{"user_id","credential_id"}` of the original pusher for a replayed
/// push, else `null`. Refs are listed force first, then delete, create,
/// fast-forward, each group in the given order (pass them sorted by name:
/// [`PushRecord::sorted_updates`]), at most [`MAX_LISTED_REFS`], each with
/// at most [`MAX_LISTED_COMMITS`] commits; a ref that does not fit with its
/// commits is listed without them, and the list stops at the first ref
/// that does not fit at all. What is left out is always stated:
/// `refs_truncated` (fewer refs than `total_refs`) and `commits_truncated`
/// (fewer commits than `total_commits`, per ref and overall).
pub fn push_json(
    project: ProjectId,
    repository: RepoId,
    updates: &[PushedRef],
    replayed_from: Option<PushReplay>,
    budget: usize,
) -> serde_json::Value {
    let mut ordered: Vec<&PushedRef> = updates.iter().collect();
    ordered.sort_by_key(|u| listing_rank(u.kind)); // stable: name order stays within a kind
    let mut used = HEADER_BYTES;
    let mut refs = Vec::new();
    let mut listed_commits: u64 = 0;
    for u in ordered.into_iter().take(MAX_LISTED_REFS) {
        let listed = u.commits.len().min(MAX_LISTED_COMMITS);
        let full = ref_entry(u, listed);
        let size = full.to_string().len() + 1;
        if used + size <= budget {
            used += size;
            listed_commits += listed as u64;
            refs.push(full);
            continue;
        }
        let bare = ref_entry(u, 0);
        let size = bare.to_string().len() + 1;
        if used + size > budget {
            break;
        }
        used += size;
        refs.push(bare);
    }
    let total_commits: u64 = updates.iter().map(|u| u.total_commits).sum();
    let count = |kind| updates.iter().filter(|u| u.kind == kind).count();
    serde_json::json!({
        "project_id": project.get(),
        "repository_id": repository.get(),
        "total_refs": updates.len(),
        "refs_truncated": refs.len() < updates.len(),
        "created": count(PushUpdateKind::Create),
        "fast_forward": count(PushUpdateKind::FastForward),
        "forced": count(PushUpdateKind::Force),
        "deleted": count(PushUpdateKind::Delete),
        "total_commits": total_commits,
        "commits_truncated": listed_commits < total_commits,
        "replayed_from": replayed_from.map(|r| serde_json::json!({
            "user_id": r.pusher.get(),
            "credential_id": r.credential_id,
        })),
        "refs": refs,
    })
}

/// Who pushed: the request and its authenticated principal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pusher {
    /// The push request's id; part of the dedup key, so a replay must pass
    /// the logged one.
    pub request_id: RequestId,
    pub user: UserId,
    /// The token used (its id), `None` for a session.
    pub credential_id: Option<i64>,
    pub source_ip: Option<IpAddr>,
    pub user_agent: Option<String>,
}

/// What [`PushRecorder::record_push`] did.
#[must_use = "Deduped wrote nothing; callers report it differently"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushRecorded {
    /// One `repository.pushed` audit row and event were written.
    Recorded,
    /// The same request id and updates were recorded before (a retry or a
    /// replay): nothing was written.
    Deduped,
}

/// Records applied pushes (forge-db implements it on `Db`; `forge serve`
/// injects it into forge-web).
pub trait PushRecorder: Send + Sync {
    /// One `repository.pushed` audit row and event in one transaction, or
    /// [`PushRecorded::Deduped`]. Errors: `FG-DB-0010
    /// reason=invalid_push_record` ([`PushRecord::check`]), `FG-REPO-0005`
    /// for an unknown repository, the store's own (`FG-DB-0001` /
    /// `FG-DB-0002`). Bounded by the change's own deadline (30 s).
    fn record_push<'a>(
        &'a self,
        pusher: &'a Pusher,
        push: &'a PushRecord,
    ) -> LookupFuture<'a, PushRecorded>;
}
