//! forge-db: the PostgreSQL layer (ADR-0002). Depends on `forge-core` and
//! `forge-errors` only (CLAUDE.md §3.9).
//!
//! - [`Db::connect`] opens the pool from `forge_core::config::DatabaseConfig`
//!   with TLS and every timeout applied (`FG-DB-0001` on failure).
//! - [`query_error`] maps a failed statement to `FG-DB-0002` (or
//!   `FG-DB-0001` when no connection could be acquired).

mod error_map;
mod pool;

pub use error_map::query_error;
pub use pool::Db;
