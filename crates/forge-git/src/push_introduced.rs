//! The commits a push introduced (M3 R5 round 4, ADR-0024 §8.3): every
//! commit its updates made reachable that no ref reached before the push.
//! The committer rule records one row per (commit, pusher) from it; a ref
//! update that brings no new commit (a tag or a branch on an existing
//! commit, a delete) adds none.
//!
//! "Before the push" is a snapshot (review round 5): [`Git::ref_snapshot`]
//! reads every ref tip (`for-each-ref`, hidden refs included, tags peeled
//! to commits, symbolic refs skipped) before receive-pack starts, and the
//! listing is one `rev-list --stdin` fed the new tips, then `^<tip>` for
//! each snapshot tip and `^<old>` for each updated ref — all on stdin,
//! never a live ref glob. A ref created after the snapshot (another push of
//! the same commit, a merge request's version pin) therefore hides nothing:
//! two concurrent pushers of one new commit both get a row. That
//! over-attribution is safe (it can only add committers, which only refuses
//! more approvals); a missed committer is the failure that matters.
//!
//! Object ids reach git only on stdin, validated; `--end-of-options` ends
//! the options defensively.

use std::time::Duration;

use forge_core::{ObjectId, RepoId};
use forge_errors::{ErrorCode, ForgeError};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::Instant;

use crate::command::Git;
use crate::push_check::CommitQuery;
use crate::storage::StorageRoot;
use crate::upload_request::is_object_id;

/// What the listing found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Introduced {
    pub commits: Vec<ObjectId>,
    /// Every new commit is listed (false: the cap, the deadline).
    pub complete: bool,
    /// Why the listing is incomplete: `cap` or `deadline` (`None` when
    /// complete).
    pub reason: Option<&'static str>,
}

/// One tip as raw bytes: a SHA-1 id in the first 20 bytes (zero-padded),
/// a SHA-256 id in all 32 (the repository's format decides; one format per
/// repository).
type Tip = [u8; 32];

/// The ref tips before a push: every ref's commit (tags peeled), each
/// commit once — sorted raw bytes, 32 per tip (review round 6: no string
/// and no hash set per ref; a million refs hold at most 32 MB).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RefSnapshot {
    tips: Vec<Tip>,
    /// Hex digits per id (40 or 64; 0 while empty).
    width: usize,
    /// The refs read (symbolic refs and refs to trees or blobs included).
    pub refs: usize,
}

impl RefSnapshot {
    /// The number of distinct commits the refs pointed at.
    pub fn len(&self) -> usize {
        self.tips.len()
    }

    /// No ref pointed at a commit.
    pub fn is_empty(&self) -> bool {
        self.tips.is_empty()
    }

    /// The distinct commits as hex, sorted.
    pub fn tip_hexes(&self) -> impl Iterator<Item = String> + '_ {
        self.tips.iter().map(|t| hex(t, self.width))
    }

    fn add(&mut self, tip: &str) {
        let mut raw = [0u8; 32];
        for (i, pair) in tip.as_bytes().chunks(2).enumerate() {
            if let (Some(slot), [hi, lo]) = (raw.get_mut(i), pair) {
                *slot = (nibble(*hi) << 4) | nibble(*lo);
            }
        }
        self.width = tip.len();
        self.tips.push(raw);
    }

    fn seal(&mut self) {
        self.tips.sort_unstable();
        self.tips.dedup();
    }
}

fn nibble(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        // Unreachable for a validated id (`is_object_id`).
        _ => 0,
    }
}

fn hex(raw: &Tip, width: usize) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(width);
    for b in raw.iter().take(width / 2) {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

/// The most refs a snapshot reads; past it the snapshot fails
/// (`FG-SYS` internal, `reason=ref_snapshot_too_large`) and the push
/// records an incomplete listing.
pub const MAX_SNAPSHOT_REFS: usize = 1_000_000;

/// `for-each-ref` fields: the ref's object and type, the peeled object and
/// type (empty unless a tag), the symbolic target (empty unless symbolic).
const SNAPSHOT_FORMAT: &str =
    "--format=%(objectname) %(objecttype) %(*objectname) %(*objecttype) %(symref)";

/// One snapshot line to the commit it contributes (`None`: a symbolic ref,
/// a ref to a tree or a blob, a tag of a tag). A missed tip only makes more
/// commits look new: over-attribution, safe.
fn snapshot_tip(line: &str) -> Option<&str> {
    let mut f = line.split(' ');
    let (oid, kind, peeled, peeled_kind, symref) =
        (f.next()?, f.next()?, f.next()?, f.next()?, f.next()?);
    if !symref.is_empty() {
        return None;
    }
    let tip = match (kind, peeled_kind) {
        ("commit", _) => oid,
        ("tag", "commit") => peeled,
        _ => return None,
    };
    is_object_id(tip).then_some(tip)
}

fn zero(hex: &str) -> bool {
    hex.bytes().all(|b| b == b'0')
}

impl Git {
    /// Every ref tip of the repository as it is now (module docs), read
    /// before receive-pack starts. Streamed; `timeout` bounds it.
    pub async fn ref_snapshot(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        principal: &str,
        timeout: Duration,
    ) -> Result<RefSnapshot, ForgeError> {
        let repo = storage.existing(id)?;
        let mut stream = self
            .command("for-each-ref")
            .arg(SNAPSHOT_FORMAT)
            .repo(&repo)
            .timeout(timeout + Duration::from_secs(5))
            .spawn_stream(principal)
            .await?;
        drop(stream.take_stdin());
        let Some(stdout) = stream.take_stdout() else {
            return Err(
                ForgeError::new(ErrorCode::Internal).with_context("reason", "stream_pipes_taken")
            );
        };
        let mut lines = BufReader::new(stdout).lines();
        let mut snapshot = RefSnapshot::default();
        let read = async {
            while let Some(line) = lines.next_line().await? {
                snapshot.refs += 1;
                if snapshot.refs > MAX_SNAPSHOT_REFS {
                    return Ok(false);
                }
                if let Some(tip) = snapshot_tip(&line) {
                    snapshot.add(tip);
                }
            }
            Ok::<bool, std::io::Error>(true)
        };
        let read = tokio::time::timeout(timeout, read).await;
        drop(lines);
        match read {
            Ok(Ok(true)) => {
                stream.finish().await?;
                snapshot.seal();
                Ok(snapshot)
            }
            Ok(Ok(false)) => {
                stream.stop_early().await?;
                Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "ref_snapshot_too_large")
                    .with_context("max_refs", MAX_SNAPSHOT_REFS)
                    .with_context("refs", snapshot.refs))
            }
            Ok(Err(e)) => {
                stream.stop_early().await?;
                Err(ForgeError::new(ErrorCode::GitProcessFailed)
                    .with_context("subcommand", "for-each-ref")
                    .with_context("reason", "stream_io_failed")
                    .with_context("refs", snapshot.refs)
                    .with_source(e))
            }
            Err(_) => {
                stream.stop_early().await?;
                Err(ForgeError::new(ErrorCode::GitTimeout)
                    .with_context("subcommand", "for-each-ref")
                    .with_context("timeout_ms", timeout.as_millis())
                    .with_context("refs", snapshot.refs))
            }
        }
    }

    /// The commits `updates` introduced relative to `before` (module
    /// docs); deletes and tags of trees or blobs bring none. `timeout`
    /// bounds the call.
    #[allow(clippy::too_many_arguments)] // one listing, every input named
    pub async fn introduced_commits(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        before: &RefSnapshot,
        updates: &[CommitQuery],
        principal: &str,
        cap: usize,
        timeout: Duration,
    ) -> Result<Introduced, ForgeError> {
        for u in updates {
            if !is_object_id(&u.new) || !is_object_id(&u.old) {
                return Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "push_check_invalid_object_id"));
            }
        }
        let started = Instant::now();
        let repo = storage.existing(id)?;
        // Only new ids that peel to a commit bring commits (a tag of a tree
        // or a blob brings none; rev-list would refuse it).
        let news: Vec<&str> = updates
            .iter()
            .filter(|u| !zero(&u.new))
            .map(|u| u.new.as_str())
            .collect();
        let peeled = if news.is_empty() {
            std::collections::BTreeMap::new()
        } else {
            self.peels_to_commit(&repo, &news, timeout).await?
        };
        let mut stdin = String::new();
        let mut any = false;
        for new in news {
            if peeled.get(new) == Some(&true) {
                stdin.push_str(new);
                stdin.push('\n');
                any = true;
            }
        }
        if !any {
            return Ok(Introduced {
                commits: Vec::new(),
                complete: true,
                reason: None,
            });
        }
        // Everything reachable before the push: the snapshot's tips and the
        // old ids (tips of the snapshot too unless the ref moved between).
        for tip in before.tip_hexes() {
            stdin.push('^');
            stdin.push_str(&tip);
            stdin.push('\n');
        }
        for u in updates {
            if !zero(&u.old) {
                stdin.push('^');
                stdin.push_str(&u.old);
                stdin.push('\n');
            }
        }
        let timeout = timeout.saturating_sub(started.elapsed());
        let mut stream = self
            .command("rev-list")
            .arg("--stdin")
            .arg("--end-of-options")
            .repo(&repo)
            .timeout(timeout + Duration::from_secs(5))
            .spawn_stream(principal)
            .await?;
        let (Some(mut input), Some(stdout)) = (stream.take_stdin(), stream.take_stdout()) else {
            return Err(
                ForgeError::new(ErrorCode::Internal).with_context("reason", "stream_pipes_taken")
            );
        };
        let deadline = Instant::now() + timeout;
        let write = async move {
            input.write_all(stdin.as_bytes()).await?;
            input.shutdown().await
        };
        let mut out = Introduced {
            commits: Vec::new(),
            complete: true,
            reason: None,
        };
        let mut lines = BufReader::new(stdout).lines();
        let read = async {
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if out.commits.len() >= cap {
                            out.complete = false;
                            out.reason = Some("cap");
                            return Ok(());
                        }
                        let oid = ObjectId::parse(line.trim()).map_err(|_| {
                            std::io::Error::new(std::io::ErrorKind::InvalidData, "not an object id")
                        })?;
                        out.commits.push(oid);
                    }
                    Ok(None) => return Ok(()),
                    Err(e) => return Err(e),
                }
            }
        };
        let joined = async { tokio::join!(write, read) };
        match tokio::time::timeout_at(deadline, joined).await {
            Ok((written, read)) => {
                drop(lines);
                if !out.complete {
                    // Stopped at the cap: git may still be writing.
                    stream.stop_early().await?;
                    return Ok(out);
                }
                stream.finish().await?;
                for r in [written, read] {
                    r.map_err(|e| {
                        ForgeError::new(ErrorCode::GitProcessFailed)
                            .with_context("subcommand", "rev-list")
                            .with_context("reason", "stream_io_failed")
                            .with_source(e)
                    })?;
                }
                Ok(out)
            }
            Err(_) => {
                drop(lines);
                stream.stop_early().await?;
                out.complete = false;
                out.reason = Some("deadline");
                Ok(out)
            }
        }
    }
}

#[cfg(test)]
#[path = "push_introduced_tests.rs"]
mod tests;
