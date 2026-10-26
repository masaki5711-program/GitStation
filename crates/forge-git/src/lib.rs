//! forge-git: the only place forge runs `git` (CLAUDE.md §3-5).
//!
//! - [`Git`] owns the git binary path, the scrubbed environment, timeouts and
//!   the concurrency limit; every subprocess goes through [`Git::command`].
//! - [`version`] enforces the minimum git version (2.38).
//! - [`storage`] derives repository paths from `RepoId` only (ADR-0009) and
//!   creates bare repositories.

mod command;
pub mod storage;
pub mod version;

pub use command::{Git, GitCommand, GitConfig, GitOutput, Revision};
pub use storage::StorageRoot;
pub use version::GitVersion;
