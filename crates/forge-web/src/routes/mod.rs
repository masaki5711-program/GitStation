//! Route handlers, one module per resource. Register them in
//! `app::registry()` — never with `Router::route`. A handler takes
//! `(state, Dispatched<R>)`: it runs only with the registry's admitted
//! proof.

pub mod git;
pub mod git_push;
pub mod groups;
pub mod health;
pub mod lists;
pub mod members;
pub mod moves;
pub mod namespaces;
pub mod project_writes;
pub mod projects;
pub mod protected_refs;
pub mod session;
pub mod shell;
pub mod users;
mod write;
