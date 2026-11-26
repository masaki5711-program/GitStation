//! forge documents (plan 13, ADR-0026): pure parsing shared by the server
//! (forge-core, forge-checks) and the `gstn` client. No I/O, no clock, no
//! randomness — bytes in, values out.
//!
//! Dependency direction (CLAUDE.md contract 9): `forge-core → forge-doc →
//! forge-errors`, and `forge-client → forge-doc`.

pub mod ai_asset;
pub mod doctype;
pub mod document;
pub mod fmt;
pub mod legacy;
pub mod lint;
pub mod render;
pub mod spec_doc;
// Plan 20 N1 (ADR-0040 §6): a person's notes project and one note.
pub mod note;
// Plan 20 W2 (ADR-0040 §5): a project wiki's first commit.
pub mod wiki;
pub mod yaml;
