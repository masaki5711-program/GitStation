//! forge-core: domain primitives and runtime foundations shared by every
//! server-side crate. Depends only on `forge-errors` (CLAUDE.md §3.9).

pub mod ids;
pub mod ref_name;
pub mod request_id;
pub mod telemetry;

pub use ids::{GroupId, ProjectId, RepoId, UserId};
pub use ref_name::RefName;
pub use request_id::RequestId;
