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
//! - [`subscribers`] runs internal event subscribers one event per
//!   transaction with [`subscribers::DerivedTx`] (`FG-DB-0008`).
//! - [`users`] creates (with the personal namespace) and disables users
//!   (`FG-REPO-0007`..`0009`, `FG-AUTH-0005`..`0007`).
//! - [`groups`] creates groups and changes their direct members; [`roles`]
//!   loads a user's roles on a group or project (`MemberRoles`, the one
//!   role computation, ADR-0010).
//! - [`projects`] creates a project with its namespace, repository, first
//!   Owner and provisioning job (`FG-REPO-0014`); [`repositories`] holds
//!   the provisioning job's definition.
//! - [`resolve`] implements `forge_core::NamespaceResolver` and
//!   `ProjectAccessLoader` on [`Db`]: path (or project id) → target and the
//!   viewer's authorization inputs in one statement (`FG-DB-0021`).

#[cfg(test)]
mod audited_doctor_tests;
#[cfg(test)]
mod audited_edge_tests;
#[cfg(test)]
mod audited_tests;
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
// ── C5: event subscribers and DerivedTx (ADR-0016 §6, §7) ──
pub mod subscribers;
// ── end C5 ──
// ── D1: users and namespaces ──
mod namespaces;
pub mod users;
#[cfg(test)]
mod users_constraint_tests;
#[cfg(test)]
mod users_schema_tests;
// ── end D1 ──
// ── D2: groups and their members ──
pub mod groups;
#[cfg(test)]
mod groups_schema_tests;
pub mod roles;
// ── end D2 ──
// ── D3: personal access tokens ──
pub mod tokens;
// ── end D3 ──
// ── D4: projects, repositories, project members, protected refs ──
pub mod projects;
#[cfg(test)]
mod projects_schema_tests;
pub mod repositories;
// ── end D4 ──
// ── D4 MR5: protected ref rules ──
pub mod protected_refs;
// ── end D4 MR5 ──
// ── M1 phase 8 MR3: the namespace resolver ──
pub mod resolve;
// ── end M1 phase 8 MR3 ──
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
#[cfg(test)]
mod test_support_tests;

pub use error_map::{query_error, read_query_error};
pub use migrate::{check_schema, migrate, status, SchemaStatus, MIGRATOR};
pub use migrator::MigratorConn;
pub use pool::{Db, ReadPool};
