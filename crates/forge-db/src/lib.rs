//! forge-db: the PostgreSQL layer (ADR-0002, ADR-0018). Depends on
//! `forge-core` and `forge-errors` only (CLAUDE.md §3.9).
//!
//! - [`Db::connect`] opens the write pool (crate-private) and the read pool
//!   ([`ReadPool`], `forge_ro`, read-only) from
//!   `forge_core::config::DatabaseConfig` with TLS and every timeout applied
//!   (`FG-DB-0001` on failure).
//! - [`Db::connect_migrator`] opens the migration connection as
//!   `forge_owner` ([`MigratorConn`]).
//! - [`query_error`] / [`read_query_error`] map a failed statement to
//!   `FG-DB-0002` (or `FG-DB-0001` / `FG-DB-0009`).
//! - [`migrate`] / [`status`] / [`check_schema`] implement the migration
//!   policy of ADR-0014 (`FG-DB-0003` / `FG-DB-0004`).
//! - [`bootstrap`] checks what `ops/db/bootstrap/` set up (`forge doctor`).
//! - [`sequencer`] numbers the outbox (`forge.assign_seq`, `forge worker`)
//!   and reports its backlog / lag (`FG-DB-0005`).
//! - [`seal`] seals the audit log (`forge.audit_seal`, the `audit.seal`
//!   job), verifies seals and reports the seal backlog (`FG-DB-0011`,
//!   `FG-DB-0015`).
//! - [`jobs`] is the job queue with fenced leases and its runner
//!   (`FG-DB-0006` / `FG-DB-0007`).

pub mod bootstrap;
pub mod change;
mod error_map;
pub mod jobs;
mod migrate;
mod migrator;
mod options;
mod pool;
#[cfg(test)]
mod schema_tests;
// ── C8 ──
pub mod seal;
// ── end C8 ──
pub mod sequencer;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use error_map::{query_error, read_query_error};
pub use migrate::{check_schema, migrate, status, SchemaStatus, MIGRATOR};
pub use migrator::MigratorConn;
pub use pool::{Db, ReadPool};
