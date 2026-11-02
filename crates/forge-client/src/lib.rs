//! `forge-client`: the typed client for REST API v1 and the `gstn` CLI built
//! on it (plan 01 §3.11, M1 phase 8b plan "gstn series").
//!
//! Dependency decision (CLAUDE.md §3.9): this crate is a *client*, outside the
//! server's layering. It depends on `forge-errors` only — the generated
//! registry of `FG-…` codes and their messages, which both sides must agree on
//! — and on no server crate (no `forge-core`, `forge-db`, `forge-auth`,
//! `forge-web`): it talks to the server over HTTP and nothing else. No server
//! crate may depend on it. `Cargo.toml` of the server crates is checked by
//! `client_depends_only_on_forge_errors` below.

pub mod app;
pub mod browse;
pub mod cli;
pub mod config;
pub mod coverage;
pub mod credential;
pub mod error;
pub mod gitcfg;
pub mod hosting;
pub mod http;
pub mod registry;
pub mod store;

#[cfg(test)]
mod tests;
