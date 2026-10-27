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

pub mod bootstrap;
mod error_map;
mod migrate;
mod migrator;
mod options;
mod pool;
#[cfg(test)]
mod schema_tests;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use error_map::{query_error, read_query_error};
pub use migrate::{check_schema, migrate, status, SchemaStatus, MIGRATOR};
pub use migrator::MigratorConn;
pub use pool::{Db, ReadPool};
