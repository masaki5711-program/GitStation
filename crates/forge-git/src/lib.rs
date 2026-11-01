//! forge-git: the only place forge runs `git` (CLAUDE.md §3-5).
//!
//! - [`Git`] owns the git binary path, the scrubbed environment, timeouts and
//!   the concurrency limit; every subprocess goes through [`Git::command`].
//! - [`version`] enforces the minimum git version (2.38).
//! - [`storage`] derives repository paths from `RepoId` only (ADR-0009) and
//!   creates bare repositories (marked with a provision token; a retry adopts
//!   only its own marked repository).
//! - [`GitCommand::spawn_stream`] runs git as a stream (Smart HTTP): separate
//!   slot pool, per-principal cap, idle and total timeouts, process-tree kill.
//! - [`smart_http`] builds upload-pack / receive-pack with fixed `-c` sets;
//!   [`pkt_line`] frames packets and side-band; [`gzip`] decodes request
//!   bodies with a decompressed-size cap.
//! - [`upload_request`] reads an upload-pack request (command, wants) and
//!   [`Git::check_wants`] checks every want is reachable from a ref before
//!   upload-pack starts (ADR-0007 option (a)).
//! - [`receive_request`] reads a receive-pack request's command section,
//!   [`receive_report`] reads / rewrites receive-pack's report and builds
//!   forge's in-band refusal, [`Git::fast_forwards`] / [`Git::new_commits`]
//!   classify a push's updates and list their commits (M1 phase 9 MR4).

mod command;
mod config;
pub mod gzip;
pub mod pkt_line;
mod process;
mod process_tree;
mod push_check;
pub mod receive_report;
pub mod receive_request;
mod redact;
mod revision;
pub mod smart_http;
pub mod storage;
mod stream;
pub mod upload_request;
pub mod version;
mod want_check;

pub use command::{Git, GitCommand, GitConfig, GitOutput};
pub use push_check::{Ancestry, CommitList, CommitQuery};
pub use revision::Revision;
pub use storage::{Provisioned, StorageRoot};
pub use stream::{GitStdin, GitStdout, GitStream, StreamEnded, StreamStats};
pub use version::GitVersion;
pub use want_check::{WantCheck, WantLimits};
