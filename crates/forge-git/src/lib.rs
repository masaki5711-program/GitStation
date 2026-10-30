//! forge-git: the only place forge runs `git` (CLAUDE.md §3-5).
//!
//! - [`Git`] owns the git binary path, the scrubbed environment, timeouts and
//!   the concurrency limit; every subprocess goes through [`Git::command`].
//! - [`version`] enforces the minimum git version (2.38).
//! - [`storage`] derives repository paths from `RepoId` only (ADR-0009) and
//!   creates bare repositories (marked with a provision token; a retry adopts
//!   only its own marked repository).

mod command;
mod process;
mod redact;
mod revision;
pub mod storage;
pub mod version;

pub use command::{Git, GitCommand, GitConfig, GitOutput};
pub use revision::Revision;
pub use storage::{Provisioned, StorageRoot};
pub use version::GitVersion;
