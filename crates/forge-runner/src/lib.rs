//! `forge-runner`: the CI runner (M4 C4, plan 14 §3.1, ADR-0029). A
//! long-running process on a separate Linux host that claims jobs from
//! forge over HTTPS with its runner token, fetches each job's pinned commit
//! with the job's own token, checks `.forge-ci.yml` against the pipeline's
//! `definition_hash`, and runs the job in a throwaway rootless Podman
//! container (argv only, no network, read-only root, killed and removed at
//! its timeout), streaming its log back.
//!
//! Dependency decision (contract 9, ADR-0029): forge-core, forge-errors and
//! forge-client only (forge-ci through `forge_client::forge_ci`); never a
//! server crate, and nothing depends on this crate.

pub mod api;
pub mod config;
pub mod git;
pub mod isolation;
pub mod job;
pub mod logsink;
pub mod podman;
pub mod process;
pub mod runner;
pub mod workspace;

#[cfg(test)]
mod job_tests;
