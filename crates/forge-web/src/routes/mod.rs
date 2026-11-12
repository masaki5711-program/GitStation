//! Route handlers, one module per resource. Register them in
//! `app::registry()` — never with `Router::route`. A handler takes
//! `(state, Dispatched<R>)`: it runs only with the registry's admitted
//! proof.

// M3 S2: the act-as confirmation and claim (ADR-0025 §5).
pub mod act_as;
pub mod assets;
pub mod auth;
pub mod browse;
pub mod checks;
pub mod content;
pub mod directory;
// Plan 13 Q13: the document lint migration setting.
pub mod doc_lint_migration;
// Simple editor E1: one file committed from the web, like a push.
pub mod file_commit;
// Plan 13 stage 3: the document API (the document view as JSON).
pub mod documents;
pub mod git;
pub mod git_push;
// Plan 06 D-5 safety net: whole copies (clone / fetch, archives) by agents.
pub mod groups;
pub mod health;
pub mod history;
pub mod lists;
pub mod members;
pub mod merge_requests;
pub mod moves;
pub mod repo_copy;
// M3 N1: the caller's notification settings.
pub mod in_app_notifications;
pub mod namespaces;
pub mod notifications;
pub mod project_writes;
pub mod projects;
pub mod protected_ref_merge_rules;
pub mod protected_refs;
pub mod service_accounts;
// M3 I1a: integrations and project bindings.
pub mod integration_bindings;
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
pub mod user_activity;
pub mod user_merge_requests;
pub mod user_sessions;
pub mod users;
// ── M3 O2: the push mirror's administration ──
pub mod push_mirror;
// ── end M3 O2 ──
// ── M4 C2: CI pipelines ──
pub mod pipelines;
// ── M4 C3: CI runners and the runner API ──
pub mod runners;
// M4 C4: a CI job's fetch of its pinned commit.
pub mod job_fetch;
// M4 C6: a project's CI variables (secrets).
pub mod ci_variables;
// M4 C6: CI artifacts (a job's upload, its needs, the project's reads).
pub mod ci_artifacts;
// ── M3 I2: project webhooks (ADR-0016 §8) ──
pub mod webhooks;
// ── end M3 I2 ──
mod write;
