//! History (M2 MR8, plan 10 §5 `log -z --format`, `diff-tree --patch`):
//! the commits of a revision (paged by a cursor), one commit with its diff
//! and a comparison of two commits.
//!
//! - **Commits** are `git log -z --format=… -n N [--skip K] --end-of-options
//!   <tip> [-- <path>]`. The tip is a resolved commit id, so the order is
//!   fixed for as long as the cursor lives: the page after a cursor starts
//!   at the commit the cursor names, found again by `--skip` and *checked*
//!   (the first record must be that commit, else the cursor does not belong
//!   to this tip and path: `FG-WEB-0006 reason=cursor_invalid rule=anchor`).
//!   Fields are separated by `0x1f`, records by NUL; names and subjects are
//!   decoded as UTF-8 with replacement characters (display text only — ids
//!   and positions never are).
//! - **A commit** is its metadata and message plus the diff against its
//!   first parent (a root commit against nothing).
//! - **A comparison** of `from` and `to` is the commits reachable from `to`
//!   and not from `from` (at most [`super::BrowseLimits::max_compare_commits`])
//!   and the tree diff from `from` to `to`; its time limit is
//!   [`super::BrowseLimits::compare_timeout`].

use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};

use super::diff::Diff;
use super::path::RepoPath;
use super::{not_resolved, object_id, Browser, Deadline};
use crate::revision::Revision;

/// Bytes of `log` output read at most (the commit cap decides first on any
/// real history).
const LOG_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// `%H`, `%P`, author name / email / date (strict ISO 8601), committer name
/// / email / date, then the subject.
const LIST_FORMAT: &str = "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%aI%x1f%cn%x1f%ce%x1f%cI%x1f%s";

/// The same fields, then the whole message in place of the subject.
const DETAIL_FORMAT: &str = "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%aI%x1f%cn%x1f%ce%x1f%cI%x1f%B";

/// One commit's metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub oid: String,
    /// In order; the first is the mainline of a merge.
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    /// Strict ISO 8601 with the author's offset.
    pub authored_at: String,
    pub committer_name: String,
    pub committer_email: String,
    pub committed_at: String,
    /// The first paragraph of the message on one line.
    pub subject: String,
}

/// One commit with its message and its diff against the first parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitDetail {
    pub info: CommitInfo,
    pub message: String,
    pub diff: Diff,
}

/// Where the next page of a history starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogCursor {
    /// The tip the history is read from (a commit id).
    pub tip: String,
    /// The last commit of the previous page.
    pub last: String,
    /// How many commits (of this tip and path) the pages so far covered.
    pub skip: usize,
}

impl LogCursor {
    const VERSION: &'static str = "v1";

    /// `v1.<tip>.<last>.<skip>`.
    pub fn encode(&self) -> String {
        format!("{}.{}.{}.{}", Self::VERSION, self.tip, self.last, self.skip)
    }

    /// The cursor a page handed out; `None` for anything else (wrong
    /// shape, ids that are not full hex, a zero or too large skip).
    pub fn parse(raw: &str, max_skip: usize) -> Option<Self> {
        let mut parts = raw.split('.');
        let (Some(version), Some(tip), Some(last), Some(skip), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return None;
        };
        if version != Self::VERSION {
            return None;
        }
        let tip = lower_id(tip)?;
        let last = lower_id(last)?;
        if skip.is_empty() || !skip.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let skip: usize = skip.parse().ok()?;
        (1..=max_skip)
            .contains(&skip)
            .then_some(Self { tip, last, skip })
    }
}

/// A full id in lower case only (the form a cursor is made with).
fn lower_id(raw: &str) -> Option<String> {
    (raw == raw.to_ascii_lowercase()).then(|| object_id(raw))?
}

/// One page of a history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitPage {
    /// The tip the page was read from.
    pub tip: String,
    pub commits: Vec<CommitInfo>,
    /// `None` on the last page.
    pub next: Option<LogCursor>,
}

/// A comparison of two commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    /// The commits of `to` that `from` does not have, newest first.
    pub commits: Vec<CommitInfo>,
    /// More commits than [`super::BrowseLimits::max_compare_commits`].
    pub commits_truncated: bool,
    /// The tree diff from `from` to `to`.
    pub diff: Diff,
}

/// A parsed `log` record: the metadata and the rest field (subject or
/// message).
struct Record {
    info: CommitInfo,
    message: String,
}

fn log_malformed(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::GitProcessFailed).with_context("reason", reason)
}

/// The first paragraph of a message on one line (git's subject).
fn subject_of(message: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in message.trim_start_matches('\n').lines() {
        if line.trim().is_empty() {
            break;
        }
        lines.push(line.trim());
    }
    lines.join(" ")
}

/// `log -z` output → records. `cut`: the output was truncated (the last,
/// partial record is dropped). A record whose ids are not ids is an error
/// (git's output is not what forge expects); the other fields are display
/// text.
fn parse_log(out: &[u8], cut: bool, detail: bool) -> Result<Vec<Record>, ForgeError> {
    let mut records: Vec<&[u8]> = out.split(|b| *b == 0).collect();
    // `-z` ends every record with a NUL: the tail is empty, or partial.
    let tail = records.pop();
    if !cut && tail.is_some_and(|t| !t.is_empty()) {
        // A separator rather than a terminator: the tail is a record.
        records.extend(tail);
    }
    let mut parsed = Vec::with_capacity(records.len());
    for record in records {
        let text = String::from_utf8_lossy(record);
        let text = text.trim_start_matches('\n');
        let fields: Vec<&str> = text.splitn(9, '\x1f').collect();
        let [oid, parents, an, ae, ad, cn, ce, cd, rest] = fields.as_slice() else {
            return Err(log_malformed("log_record_fields"));
        };
        let oid = object_id(oid).ok_or_else(|| log_malformed("log_record_oid"))?;
        let mut parent_ids = Vec::new();
        for p in parents.split_whitespace() {
            parent_ids.push(object_id(p).ok_or_else(|| log_malformed("log_record_parent"))?);
        }
        let (subject, message) = if detail {
            (subject_of(rest), (*rest).to_owned())
        } else {
            ((*rest).to_owned(), String::new())
        };
        parsed.push(Record {
            info: CommitInfo {
                oid,
                parents: parent_ids,
                author_name: (*an).to_owned(),
                author_email: (*ae).to_owned(),
                authored_at: (*ad).to_owned(),
                committer_name: (*cn).to_owned(),
                committer_email: (*ce).to_owned(),
                committed_at: (*cd).to_owned(),
                subject,
            },
            message,
        });
    }
    Ok(parsed)
}

impl Browser {
    /// One page of the history of `tip` (a commit id), limited to `path`
    /// (the root: everything). `after`: the previous page's cursor, whose
    /// tip must be `tip`. `limit` is clamped to
    /// `1..=max_log_page`.
    pub async fn commits(
        &self,
        repo: RepoId,
        tip: &str,
        path: &RepoPath,
        limit: usize,
        after: Option<&LogCursor>,
    ) -> Result<CommitPage, ForgeError> {
        let deadline = Deadline::new(self.limits.timeout);
        let limit = limit.clamp(1, self.limits.max_log_page.max(1));
        let tip = object_id(tip).ok_or_else(|| not_resolved("ref_not_found"))?;
        if after.is_some() {
            // A cursor names its own tip: it must be a commit of this
            // repository (not a blob or an id that does not exist).
            self.commit_of(repo, &tip, deadline).await?;
        }
        // The anchor (the previous page's last commit) is read again and
        // dropped: it proves the cursor belongs to this tip and path.
        let skip = after.map_or(0, |c| c.skip.saturating_sub(1));
        let take = limit + 1 + usize::from(after.is_some());
        let dir = self.storage.existing(repo)?;
        let _slot = self.claim(repo)?;
        let mut cmd = self
            .git
            .command("log")
            .arg("-z")
            .arg(LIST_FORMAT)
            .arg("--no-color")
            .arg("--no-decorate")
            .arg("-n")
            .arg_owned(take.to_string());
        if skip > 0 {
            cmd = cmd.arg("--skip").arg_owned(skip.to_string());
        }
        cmd = cmd.revision(Revision::object_id(&tip)?);
        if !path.is_root() {
            cmd = cmd.path(path.as_str());
        }
        let (out, cut) = cmd
            .repo(&dir)
            .timeout(deadline.left()?)
            .output_truncated(LOG_OUTPUT_BYTES)
            .await?;
        let mut records = parse_log(&out.stdout, cut, false)?;
        if let Some(cursor) = after {
            let anchored = records.first().is_some_and(|r| r.info.oid == cursor.last);
            if !anchored {
                return Err(ForgeError::new(ErrorCode::RequestInvalid)
                    .with_context("reason", "cursor_invalid")
                    .with_context("rule", "anchor"));
            }
            records.remove(0);
        }
        let more = records.len() > limit || cut;
        records.truncate(limit);
        let first_skip = after.map_or(0, |c| c.skip);
        let next = match records.last() {
            Some(last) if more => Some(LogCursor {
                tip: tip.clone(),            // clone: the page keeps the tip too
                last: last.info.oid.clone(), // clone: the cursor owns its id
                skip: first_skip + records.len(),
            }),
            _ => None,
        };
        Ok(CommitPage {
            tip,
            commits: records.into_iter().map(|r| r.info).collect(),
            next,
        })
    }

    /// The commit `oid` (a full id) with its message and its diff against
    /// the first parent.
    pub async fn commit(&self, repo: RepoId, oid: &str) -> Result<CommitDetail, ForgeError> {
        let deadline = Deadline::new(self.limits.timeout);
        let oid = object_id(oid).ok_or_else(|| not_resolved("ref_not_found"))?;
        self.commit_of(repo, &oid, deadline).await?;
        let dir = self.storage.existing(repo)?;
        let (out, cut) = {
            let _slot = self.claim(repo)?;
            self.git
                .command("log")
                .arg("-z")
                .arg(DETAIL_FORMAT)
                .arg("--no-color")
                .arg("--no-decorate")
                .arg("-n")
                .arg("1")
                .revision(Revision::object_id(&oid)?)
                .repo(&dir)
                .timeout(deadline.left()?)
                .output_truncated(LOG_OUTPUT_BYTES)
                .await?
        };
        let mut records = parse_log(&out.stdout, cut, true)?;
        let record = match (records.pop(), records.is_empty()) {
            (Some(r), true) if r.info.oid == oid => r,
            _ => return Err(log_malformed("commit_record_missing")),
        };
        let base = record.info.parents.first().map(String::as_str);
        let diff = self.diff(repo, base, &oid, deadline).await?;
        Ok(CommitDetail {
            info: record.info,
            message: record.message,
            diff,
        })
    }

    /// `from` → `to` (both commit ids): the commits `to` has and `from`
    /// does not, and the tree diff.
    pub async fn compare(
        &self,
        repo: RepoId,
        from: &str,
        to: &str,
    ) -> Result<Comparison, ForgeError> {
        let deadline = Deadline::new(self.limits.compare_timeout);
        let from = object_id(from).ok_or_else(|| not_resolved("ref_not_found"))?;
        let to = object_id(to).ok_or_else(|| not_resolved("ref_not_found"))?;
        let dir = self.storage.existing(repo)?;
        let (out, cut) = {
            let _slot = self.claim(repo)?;
            self.git
                .command("log")
                .arg("-z")
                .arg(LIST_FORMAT)
                .arg("--no-color")
                .arg("--no-decorate")
                .arg("-n")
                .arg_owned((self.limits.max_compare_commits + 1).to_string())
                .revision(Revision::object_id(&to)?)
                .revision(Revision::object_id(&from)?.excluding())
                .repo(&dir)
                .timeout(deadline.left()?)
                .output_truncated(LOG_OUTPUT_BYTES)
                .await?
        };
        let mut commits: Vec<CommitInfo> = parse_log(&out.stdout, cut, false)?
            .into_iter()
            .map(|r| r.info)
            .collect();
        let commits_truncated = commits.len() > self.limits.max_compare_commits || cut;
        commits.truncate(self.limits.max_compare_commits);
        let diff = self.diff(repo, Some(&from), &to, deadline).await?;
        Ok(Comparison {
            commits,
            commits_truncated,
            diff,
        })
    }
}
