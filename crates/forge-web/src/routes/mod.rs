//! Route handlers, one module per resource. Register them in
//! `app::registry()` — never with `Router::route`. A handler takes
//! `(state, Dispatched<R>)`: it runs only with the registry's admitted
//! proof.

pub mod assets;
pub mod auth;
pub mod browse;
pub mod checks;
pub mod content;
pub mod directory;
pub mod git;
pub mod git_push;
pub mod groups;
pub mod health;
pub mod history;
pub mod lists;
pub mod members;
pub mod merge_requests;
pub mod moves;
// M3 N1: the caller's notification settings.
pub mod namespaces;
pub mod notifications;
pub mod project_writes;
pub mod projects;
pub mod protected_ref_merge_rules;
pub mod protected_refs;
pub mod service_accounts;
// M3 I1a: integrations and project bindings.
pub mod integrations;
// M3 I1c: the event feed of an integration.
pub mod integration_dev_status;
pub mod integration_feed;
pub mod session;
pub mod shell;
// M3 X1: `.forge/repo.yml` and its spec references.
pub mod spec_refs;
pub mod specs;
// M3 X5: the context API (plan 03 §6).
pub mod context;
pub mod tokens;
pub mod user_sessions;
pub mod users;
// ── M3 I2: project webhooks (ADR-0016 §8) ──
pub mod webhooks;
// ── end M3 I2 ──
mod write;
