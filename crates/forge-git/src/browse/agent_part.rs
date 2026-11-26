//! The reads the agent marketplace's publish step needs (plan 16 A5,
//! ADR-0032 Amendment A5), bounded like the rest of the browse API (one
//! deadline, the per-repository cap, an output cap):
//!
//! ```text
//! git for-each-ref / ls-remote of the cached ref list   (a tag by its full name)
//! git ls-tree -r -z --long --full-tree --end-of-options <commit> -- <dir>
//! git cat-file tag <tag object>                        (the tagger)
//! git log -1 --format=%ae%x00%ce --end-of-options <commit>
//! ```
//!
//! Unlike [`Browser::blobs_under`], the part listing keeps symlinks and
//! submodules: the part lint refuses them instead of never seeing them.
//! Only resolved object ids and validated paths reach git.

use forge_core::{RefName, RepoId};
use forge_errors::{ErrorCode, ForgeError};

use super::path::RepoPath;
use super::refs::RefEntry;
use super::tree::{log_skipped, parse_tree, EntryKind};
use super::{Browser, Deadline, TREE_OUTPUT_BYTES};
use crate::revision::Revision;

/// One entry of a part directory (`ls-tree -r`: no trees).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartEntry {
    /// The path below the listed directory.
    pub path: String,
    pub kind: EntryKind,
    /// The octal mode as git prints it.
    pub mode: String,
    pub oid: String,
    /// Blobs and symlinks.
    pub size: Option<u64>,
}

/// Every entry under a directory of a commit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartListing {
    pub entries: Vec<PartEntry>,
    /// More than the cap (or the output was cut).
    pub truncated: bool,
}

/// The identities behind a tag, as git records them (self-declared:
/// the caller maps them to accounts; the pusher comes from the audit).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagIdentities {
    /// An annotated tag's tagger (`None` for a lightweight tag).
    pub tagger_email: Option<String>,
    pub author_email: String,
    pub committer_email: String,
}

/// The output of one `cat-file tag` or `log -1` (identities and a message
/// head; the rest is cut).
const IDENTITY_OUTPUT_BYTES: usize = 64 * 1024;

fn unreadable(detail: &'static str, repo: RepoId) -> ForgeError {
    ForgeError::new(ErrorCode::GitProcessFailed)
        .with_context("reason", "agent_part_output_unreadable")
        .with_context("detail", detail)
        .with_context("repo_id", repo.get())
}

/// The address between `<` and `>` of an identity line's value.
fn email_of(identity: &str) -> Option<String> {
    let start = identity.find('<')?;
    let end = identity[start..].find('>')? + start;
    let email = identity.get(start + 1..end)?.trim();
    (!email.is_empty()).then(|| email.to_owned())
}

impl Browser {
    /// The tag `name` (a full `refs/tags/…` name) as the ref list has it:
    /// the object it names and the commit it peels to (`None`: no such
    /// tag).
    pub async fn find_tag(
        &self,
        repo: RepoId,
        name: &RefName,
    ) -> Result<Option<RefEntry>, ForgeError> {
        let deadline = Deadline::new(self.limits.timeout);
        let candidates = [name.clone()]; // clone: the lookup takes a list
        let list = self.lookup(repo, &candidates, deadline).await?;
        Ok(list.find(name.as_str()).cloned())
    }

    /// Every entry (files, symlinks, submodules) under `dir` of `commit`,
    /// at most `max` (more: `truncated`), paths relative to `dir`. A
    /// directory that is not there lists nothing; the root is refused
    /// (`FG-SYS-0001 reason=part_dir_root`).
    pub async fn part_entries(
        &self,
        repo: RepoId,
        commit: &str,
        dir: &RepoPath,
        max: usize,
    ) -> Result<PartListing, ForgeError> {
        if dir.is_root() {
            return Err(
                ForgeError::new(ErrorCode::Internal).with_context("reason", "part_dir_root")
            );
        }
        let deadline = Deadline::new(self.limits.timeout);
        let storage = self.storage.existing(repo)?;
        let _slot = self.claim(repo, deadline).await?;
        let prefix = format!("{}/", dir.as_str().trim_end_matches('/'));
        let (out, cut) = self
            .git
            .command("ls-tree")
            .arg("-r")
            .arg("-z")
            .arg("--long")
            .arg("--full-tree")
            .revision(Revision::object_id(commit)?)
            .path(dir.as_str().trim_end_matches('/'))
            .repo(&storage)
            .timeout(deadline.left()?)
            .output_truncated(TREE_OUTPUT_BYTES)
            .await?;
        let parsed = parse_tree(&out.stdout, usize::MAX);
        log_skipped(repo, &parsed);
        let mut listing = PartListing {
            entries: Vec::new(),
            truncated: cut || parsed.truncated || parsed.malformed > 0 || parsed.not_utf8 > 0,
        };
        for e in parsed.entries {
            let Some(path) = e.name.strip_prefix(&prefix) else {
                // A literal pathspec lists only what is below `dir`.
                return Err(unreadable("entry_outside_dir", repo));
            };
            if listing.entries.len() >= max {
                listing.truncated = true;
                break;
            }
            listing.entries.push(PartEntry {
                path: path.to_owned(),
                kind: e.kind,
                mode: e.mode,
                oid: e.oid,
                size: e.size,
            });
        }
        Ok(listing)
    }

    /// The tagger of `tag_object` (when it is not `commit` itself: an
    /// annotated tag) and the author and committer of `commit`.
    pub async fn tag_identities(
        &self,
        repo: RepoId,
        tag_object: &str,
        commit: &str,
    ) -> Result<TagIdentities, ForgeError> {
        let deadline = Deadline::new(self.limits.timeout);
        let tag = Revision::object_id(tag_object)?;
        let commit = Revision::object_id(commit)?;
        let storage = self.storage.existing(repo)?;
        let _slot = self.claim(repo, deadline).await?;
        let tagger_email = if tag.as_str() == commit.as_str() {
            None
        } else {
            let (out, _cut) = self
                .git
                .command("cat-file")
                .arg("tag")
                .revision(tag)
                .repo(&storage)
                .timeout(deadline.left()?)
                .output_truncated(IDENTITY_OUTPUT_BYTES)
                .await?;
            let text = String::from_utf8_lossy(&out.stdout);
            // The header ends at the first empty line.
            let header = text.split("\n\n").next().unwrap_or_default();
            header
                .lines()
                .find_map(|l| l.strip_prefix("tagger "))
                .and_then(email_of)
        };
        let (out, _cut) = self
            .git
            .command("log")
            .arg("-1")
            .arg("--format=%ae%x00%ce")
            .arg("--no-color")
            .revision(commit)
            .repo(&storage)
            .timeout(deadline.left()?)
            .output_truncated(IDENTITY_OUTPUT_BYTES)
            .await?;
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().next().unwrap_or_default();
        let (author, committer) = line
            .split_once('\0')
            .ok_or_else(|| unreadable("log_identities", repo))?;
        Ok(TagIdentities {
            tagger_email,
            author_email: author.trim().to_owned(),
            committer_email: committer.trim().to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::email_of;

    #[test]
    fn the_address_is_read_between_the_brackets() {
        assert_eq!(
            email_of("Alice <alice@example.test> 1700000000 +0000").as_deref(),
            Some("alice@example.test")
        );
        assert_eq!(email_of("Alice <> 1 +0000"), None);
        assert_eq!(email_of("no address"), None);
    }
}
