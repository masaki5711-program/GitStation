//! `forge-client`: the typed client for REST API v1 and the `gstn` CLI built
//! on it (plan 01 §3.11, M1 phase 8b plan "gstn series").
//!
//! Dependency decision (CLAUDE.md §3.9, ADR-0020 and its Amendment, ADR-0026
//! §8): this crate is a *client*, outside the server's layering. It depends on
//! `forge-errors` — the generated registry of `FG-…` codes and their messages,
//! which both sides must agree on — and on `forge-doc`, the pure document
//! parsing, formatting and lint `gstn doc` shares with the server's
//! `forge/doc-lint` — and on `forge-ci`, the pure `.forge-ci.yml` lint and
//! plan of `gstn ci` (contract 9, ADR-0020 Amendment M4 C1); on no server crate (no `forge-core`, `forge-db`,
//! `forge-auth`, `forge-web`): it talks to the server over HTTP and nothing
//! else. No server crate may depend on it. `Cargo.toml` of the server crates
//! is checked by `client_depends_only_on_forge_errors_and_forge_doc` below.

/// The pure `.forge-ci.yml` parse and lint, re-exported for the runner
/// binary (M4 C4, contract 9: `forge-runner` depends on forge-client, not
/// on forge-ci directly).
pub use forge_ci;

pub mod activity;
pub mod admin;
pub mod app;
pub mod browse;
pub mod checks;
pub mod ci;
pub mod cli;
pub mod config;
pub mod conflicts;
pub mod coverage;
pub mod credential;
// Plan 13 stage 2: `gstn doc fmt` / `gstn doc lint` (local only).
pub mod doc;
pub mod error;
pub mod gitcfg;
pub mod hosting;
pub mod http;
pub mod integrations;
pub mod mr_approvals;
pub mod mr_conditions;
pub mod mr_discussions;
pub mod mr_merge;
pub mod mr_work_items;
pub mod notifications;
pub mod registry;
pub mod review;
pub mod specs;
// M3 X5: the context API and the MCP server; its tools are the agent tools
// manifest (docs/api/agent-tools.toml).
pub mod agent_tools;
pub mod context;
pub mod mcp;
pub mod store;
// M3 I2: project webhooks.
pub mod webhooks;
// M3 O2: the push mirror to GitLab.
pub mod mirror;
// M4 C2: CI pipelines.
pub mod merge_queue;
pub mod pipelines;
// Plan 13 Q13: the document lint migration setting.
pub mod doc_migration;
pub mod doc_view;

#[cfg(test)]
mod agent_tools_tests;
#[cfg(test)]
mod tests;
