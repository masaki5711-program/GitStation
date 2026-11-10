//! The older structured specification format (M3 X4, `spec:` front matter
//! and `::: req` blocks) read through the plan 13 parser (ADR-0026 §7,
//! plan 13 stage 2 MR2c).
//!
//! A document whose front matter has `spec:` and no `doctype:` is the
//! older format: it is the type `spec.system` with the id `spec`'s value.
//! [`requirements`] gives its requirements as [`crate::spec_doc`] does, so
//! the two parsers can be compared (`legacy_tests.rs`: on every generated
//! document that the older parser reads without a problem, both give the
//! same requirements — the condition before `spec_doc`'s body reading is
//! replaced).
//!
//! Known, intended differences (the newer parser is stricter; they matter
//! only for documents the older parser already reports or that the older
//! format never had):
//! - a `:::` line inside an HTML comment is no block marker;
//! - a container with an id other than `req` (`::: note N-1`) counts as a
//!   block with an id, so a `req` inside it is `block_nested`;
//! - a block kind that is no lower-case word (`::: Req`) is
//!   `block_kind_invalid`;
//! - an unclosed plain container (`::: note` never closed) is
//!   `block_unclosed` (the older parser says nothing).

use crate::document::Document;
use crate::spec_doc::{Priority, Requirement, RequirementId};

/// The type of the older format (ADR-0026 §7).
pub const SPEC_SYSTEM: &str = "spec.system";

/// Whether `doc` is in the older format: `spec:` and no `doctype:`.
pub fn is_legacy(doc: &Document) -> bool {
    doc.meta_field("doctype").is_none() && doc.meta_field("spec").is_some()
}

/// The older format's id and type: `(spec, "spec.system")`.
pub fn identity(doc: &Document) -> Option<(&str, &'static str)> {
    if !is_legacy(doc) {
        return None;
    }
    Some((doc.meta_text("spec")?, SPEC_SYSTEM))
}

/// The requirements of a document as [`crate::spec_doc`] reads them: every
/// `req` block with a valid requirement id, its priority, and deprecated
/// when the block says so or the document's `status:` is `deprecated`.
pub fn requirements(doc: &Document) -> Vec<Requirement> {
    let document_deprecated = doc.meta_text("status") == Some("deprecated");
    doc.blocks
        .iter()
        .filter(|b| b.kind == "req")
        .filter_map(|b| {
            let id = RequirementId::parse(b.id.as_deref()?)?;
            Some(Requirement {
                id,
                line: b.line,
                end_line: b.end_line,
                priority: b.attr("priority").and_then(Priority::parse),
                deprecated: document_deprecated || b.deprecated(),
                body: b.body.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
#[path = "legacy_tests.rs"]
mod tests;
