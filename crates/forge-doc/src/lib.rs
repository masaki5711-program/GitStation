//! forge documents (plan 13, ADR-0026): pure parsing shared by the server
//! (forge-core, forge-checks) and the `gstn` client. No I/O, no clock, no
//! randomness — bytes in, values out.
//!
//! Dependency direction (CLAUDE.md contract 9): `forge-core → forge-doc →
//! forge-errors`, and `forge-client → forge-doc`.

pub mod doctype;
pub mod document;
pub mod fmt;
pub mod legacy;
pub mod lint;
pub mod spec_doc;
pub mod yaml;
