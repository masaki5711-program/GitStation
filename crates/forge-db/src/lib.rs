//! forge-db: the PostgreSQL layer (ADR-0002). Depends on `forge-core` and
//! `forge-errors` only (CLAUDE.md §3.9).
//!
//! - [`Db::connect`] opens the pool from `forge_core::config::DatabaseConfig`
//!   with TLS and every timeout applied (`FG-DB-0001` on failure).
//! - [`query_error`] maps a failed statement to `FG-DB-0002` (or
//!   `FG-DB-0001` when no connection could be acquired).
//! - [`migrate`] / [`status`] / [`check_schema`] implement the migration
//!   policy of ADR-0014 (`FG-DB-0003` / `FG-DB-0004`).

mod error_map;
mod migrate;
mod pool;
#[cfg(test)]
mod test_support;

pub use error_map::query_error;
pub use migrate::{check_schema, migrate, status, SchemaStatus, MIGRATOR};
pub use pool::Db;
