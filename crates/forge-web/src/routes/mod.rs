//! Route handlers, one module per resource. Register them in
//! `app::registry()` — never with `Router::route`. A handler takes
//! `(state, Dispatched<R>)`: it runs only with the registry's admitted
//! proof.

pub mod git;
pub mod git_push;
pub mod health;
pub mod namespaces;
pub mod projects;
pub mod shell;
