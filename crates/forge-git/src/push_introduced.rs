//! The commits a push introduced (M3 R5 round 4, ADR-0024 §8.3): every
//! commit its updates made reachable that no ref reached before the push —
//! one `rev-list --stdin <new>… --not --exclude=<every updated ref>
//! --glob=refs/*` with `^<old>` of each updated ref (the refs as they were
//! before the push), streamed, at most `cap` kept. The committer rule
//! records one row per (commit, pusher) from it; a ref update that brings no
//! new commit (a tag or a branch on an existing commit, a delete) adds none.
//!
//! Object ids reach git only on stdin, validated; ref names only as
//! `--exclude=<validated ref name>`.

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
}

fn zero(hex: &str) -> bool {
    hex.bytes().all(|b| b == b'0')
}

impl Git {
    /// The commits `updates` introduced (module docs); deletes and tags of
    /// trees or blobs bring none. `timeout` bounds the call.
    pub async fn introduced_commits(
        &self,
        storage: &StorageRoot,
        id: RepoId,
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
            });
        }
        for u in updates {
            if !zero(&u.old) {
                stdin.push('^');
                stdin.push_str(&u.old);
                stdin.push('\n');
            }
        }
        let timeout = timeout.saturating_sub(started.elapsed());
        let mut cmd = self.command("rev-list").arg("--stdin").arg("--not");
        for u in updates {
            cmd = cmd.exclude_ref(&u.name);
        }
        // `--glob=refs/*`, not `--all` (HEAD would hide the pushed branch).
        cmd = cmd.arg("--glob=refs/*");
        let mut stream = cmd
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
        };
        let mut lines = BufReader::new(stdout).lines();
        let read = async {
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if out.commits.len() >= cap {
                            out.complete = false;
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
                Ok(out)
            }
        }
    }
}

#[cfg(test)]
#[path = "push_introduced_tests.rs"]
mod tests;
