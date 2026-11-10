//! The older block scanner of [`super`], frozen as it was before its body
//! reading moved to [`crate::document`] (ADR-0026 "one parser", 2026-11-10).
//! Test only: the reference the property test in `spec_doc_tests.rs`
//! compares the new reading against (requirements and problems), so any
//! change of a spec-lint answer shows. Never used by production code.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use super::{
    container_of, fence_of, Fence, Priority, Requirement, RequirementId, SpecDocument,
    MAX_CONTAINER_DEPTH, MAX_REQUIREMENTS_PER_DOCUMENT, MAX_REQUIREMENT_BODY_BYTES,
};

/// A container being read.
enum Open {
    /// A requirement: its id (when valid), opening line, attributes and
    /// body lines.
    Req {
        id: Option<RequirementId>,
        line: usize,
        priority: Option<Priority>,
        deprecated: bool,
        body: Vec<String>,
    },
    /// Any other container (`::: note`), or a nested `::: req`.
    Other,
}
/// Read the blocks into `doc`; whether any `::: req` line was seen.
pub(super) fn blocks(lines: &[&str], start: usize, doc: &mut SpecDocument) -> bool {
    let mut saw_requirement = false;
    let mut stack: Vec<Open> = Vec::new();
    let mut fence: Option<Fence> = None;
    // Review X4: where the open fence started, and whether it hides a
    // requirement block (an unclosed fence is never a silent drop).
    let mut fence_line = 0usize;
    let mut req_in_fence = false;
    let mut last_line = start;
    for (i, raw) in lines.iter().enumerate().skip(start) {
        let line = i + 1;
        last_line = line;
        if let Some(open) = &fence {
            let closes = fence_of(raw).is_some_and(|f| {
                f.0 == open.0 && f.1 >= open.1 && raw.trim().bytes().all(|b| b == open.0)
            });
            if closes {
                fence = None;
                req_in_fence = false;
            } else if matches!(container_of(raw), Some(Some(("req", _)))) {
                req_in_fence = true;
            }
            push_body(&mut stack, raw);
            continue;
        }
        if let Some(f) = fence_of(raw) {
            fence = Some(f);
            fence_line = line;
            push_body(&mut stack, raw);
            continue;
        }
        match container_of(raw) {
            None => push_body(&mut stack, raw),
            Some(None) => match stack.pop() {
                None => doc.problem(line, "close_unmatched"),
                Some(Open::Other) => {}
                Some(Open::Req {
                    id,
                    line: opened,
                    priority,
                    deprecated,
                    body,
                }) => finish(doc, (id, opened, line, true), priority, deprecated, &body),
            },
            Some(Some((name, attrs))) => {
                if stack.len() >= MAX_CONTAINER_DEPTH {
                    doc.problem(line, "too_deep");
                    stack.push(Open::Other);
                    continue;
                }
                if name != "req" {
                    stack.push(Open::Other);
                    continue;
                }
                saw_requirement = true;
                if stack.iter().any(|o| matches!(o, Open::Req { .. })) {
                    doc.problem(line, "block_nested");
                    stack.push(Open::Other);
                    continue;
                }
                stack.push(open_req(doc, line, attrs));
            }
        }
    }
    if fence.is_some() {
        doc.problem(fence_line, "unclosed_fence");
        saw_requirement |= req_in_fence;
    }
    // Unclosed containers: a requirement is still kept (its problem says
    // where it opened).
    while let Some(open) = stack.pop() {
        if let Open::Req {
            id,
            line,
            priority,
            deprecated,
            body,
        } = open
        {
            doc.problem(line, "block_unclosed");
            finish(
                doc,
                (id, line, last_line, false),
                priority,
                deprecated,
                &body,
            );
        }
    }
    saw_requirement
}

fn push_body(stack: &mut [Open], raw: &str) {
    // The innermost requirement takes the line (a nested container's text
    // is its text too).
    if let Some(Open::Req { body, .. }) = stack
        .iter_mut()
        .rev()
        .find(|o| matches!(o, Open::Req { .. }))
    {
        body.push(raw.to_owned());
    }
}

fn open_req(doc: &mut SpecDocument, line: usize, attrs: &str) -> Open {
    let mut tokens = attrs.split_whitespace();
    let id = match tokens.next() {
        None => {
            doc.problem(line, "requirement_id_missing");
            None
        }
        Some(raw) => {
            let id = RequirementId::parse(raw);
            if id.is_none() {
                doc.problem(line, "requirement_id_invalid");
            }
            id
        }
    };
    let mut priority = None;
    let mut status = None;
    for token in tokens {
        let Some((key, value)) = token.split_once('=') else {
            doc.problem(line, "attribute_invalid");
            continue;
        };
        match key {
            "priority" => {
                if priority.is_some() {
                    doc.problem(line, "attribute_duplicate");
                    continue;
                }
                match Priority::parse(value) {
                    Some(p) => priority = Some(p),
                    None => doc.problem(line, "priority_invalid"),
                }
            }
            "status" => {
                if status.is_some() {
                    doc.problem(line, "attribute_duplicate");
                    continue;
                }
                match value {
                    "active" => status = Some(false),
                    "deprecated" => status = Some(true),
                    _ => doc.problem(line, "requirement_status_invalid"),
                }
            }
            _ => doc.problem(line, "attribute_unknown"),
        }
    }
    Open::Req {
        id,
        line,
        priority,
        deprecated: status.unwrap_or(false),
        body: Vec::new(),
    }
}

/// Keep a read block: `(id, line, end_line, closed)`; an unclosed block's
/// one problem is `block_unclosed` (its body may simply not have started).
fn finish(
    doc: &mut SpecDocument,
    (id, line, end_line, closed): (Option<RequirementId>, usize, usize, bool),
    priority: Option<Priority>,
    deprecated: bool,
    body: &[String],
) {
    let body = body.join("\n").trim().to_owned();
    if body.is_empty() && closed {
        doc.problem(line, "body_empty");
    } else if body.len() > MAX_REQUIREMENT_BODY_BYTES {
        doc.problem(line, "body_too_long");
    }
    let Some(id) = id else {
        return;
    };
    if doc.requirements.len() >= MAX_REQUIREMENTS_PER_DOCUMENT {
        doc.problem(line, "too_many_requirements");
        return;
    }
    doc.requirements.push(Requirement {
        id,
        line,
        end_line,
        priority,
        deprecated,
        body,
    });
}

/// [`super::parse_spec_document`] with the older scanner.
pub(super) fn parse_old(bytes: &[u8]) -> SpecDocument {
    super::parse_with(bytes, blocks)
}

// ── the older and the newer reading agree ──────────────────────────────

use proptest::prelude::*;

/// One line of a generated document (synthetic): the lines the older
/// format uses, well-formed and broken. HTML comments are left out: a
/// `:::` line inside one is the documented difference (`crate::legacy`).
fn line() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        4 => Just("::: req ZEVA-CFG-001"),
        2 => Just("::: req ZEVA-CFG-002 priority=must"),
        1 => Just("::: req AB-12-X3 priority=could status=deprecated"),
        1 => Just("::: req Q-9 status=active"),
        1 => Just(":::: req Q-10"),
        1 => Just(":::req Q-11 priority=wont"),
        1 => Just("::: req"),
        1 => Just("::: req a-1"),
        1 => Just("::: req PBI-1"),
        1 => Just("::: req A-1 must"),
        1 => Just("::: req A-1 color=red"),
        1 => Just("::: req A-1 priority=high"),
        1 => Just("::: req A-1 status=gone"),
        1 => Just("::: req A-1 priority=must priority=should"),
        1 => Just("::: req A-1 status=active status=deprecated"),
        1 => Just("::: req A-1 priority=\"must\""),
        1 => Just("::: req A-1 note=\"a b\""),
        1 => Just("::: req A-2 =x"),
        1 => Just("::: req priority=must"),
        3 => Just("::: note"),
        1 => Just("::: note N-1"),
        1 => Just("::: Req B-1"),
        1 => Just("::: warning kind=\"x"),
        5 => Just(":::"),
        1 => Just("::: "),
        1 => Just("```"),
        1 => Just("```markdown"),
        1 => Just("~~~~"),
        1 => Just("``` a`b"),
        6 => Just("要件の本文。"),
        2 => Just(""),
        1 => Just("  字下げの行"),
        1 => Just("- 箇条 [[ZEVA-CFG-001]] と [[bad id]]"),
        1 => Just("`::: req` は例"),
        1 => Just("<b>太字</b>"),
        1 => Just("## 見出し"),
    ]
}

fn front() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        3 => Just("---\nspec: a\ntitle: t\nstatus: approved\n---\n"),
        1 => Just("---\nspec: a\ntitle: t\nstatus: deprecated\n---\n"),
        1 => Just("---\nspec: a\ntitle: t\nstatus: nope\n---\n"),
        1 => Just("---\nspec: a\n"),
        1 => Just(""),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    /// ADR-0026 §7: the body read through the newer parser answers as the
    /// older scanner did — the same requirements and the same problems —
    /// on every generated document (broken ones included).
    #[test]
    fn the_newer_reading_gives_the_older_answers(
        front in front(),
        lines in prop::collection::vec(line(), 0..40),
        crlf in any::<bool>(),
    ) {
        let mut text = format!("{front}{}\n", lines.join("\n"));
        if crlf {
            text = text.replace('\n', "\r\n");
        }
        let old = parse_old(text.as_bytes());
        let new = super::parse_spec_document(text.as_bytes());
        prop_assert_eq!(&new, &old, "{}", text);
    }

    /// The caps (problems kept, requirements read) hold the same way.
    #[test]
    fn the_caps_keep_the_same_problems(n in 190usize..230) {
        let body = "::: req A-1 x\n:::\n".repeat(n);
        let text = format!("---\nspec: a\ntitle: t\nstatus: draft\n---\n{body}");
        prop_assert_eq!(super::parse_spec_document(text.as_bytes()), parse_old(text.as_bytes()));
    }
}

#[test]
fn too_many_requirements_are_cut_the_same_way() {
    let body: String = (0..MAX_REQUIREMENTS_PER_DOCUMENT + 3)
        .map(|i| format!("::: req A-{i}\nx\n:::\n"))
        .collect();
    let text = format!("---\nspec: a\ntitle: t\nstatus: draft\n---\n{body}");
    let new = super::parse_spec_document(text.as_bytes());
    assert_eq!(new, parse_old(text.as_bytes()));
    assert_eq!(new.requirements.len(), MAX_REQUIREMENTS_PER_DOCUMENT);
}

#[test]
fn deep_nesting_is_too_deep_the_same_way() {
    let text = format!(
        "---\nspec: a\ntitle: t\nstatus: draft\n---\n{}::: req A-1\nx\n:::\n{}",
        "::: note\n".repeat(MAX_CONTAINER_DEPTH + 1),
        ":::\n".repeat(MAX_CONTAINER_DEPTH + 1)
    );
    let new = super::parse_spec_document(text.as_bytes());
    assert_eq!(new, parse_old(text.as_bytes()));
    assert!(new.problems.iter().any(|p| p.rule == "too_deep"));
}

/// The one documented difference that changes an answer (`crate::legacy`):
/// a `:::` line inside an HTML comment is no marker for the newer parser,
/// so a requirement written there is not read (the older scanner read it).
#[test]
fn a_marker_inside_an_html_comment_is_the_documented_difference() {
    let text = "---\nspec: a\ntitle: t\nstatus: draft\n---\n<!--\n::: req A-1\nx\n:::\n-->\n";
    let old = parse_old(text.as_bytes());
    let new = super::parse_spec_document(text.as_bytes());
    assert_eq!(old.requirements.len(), 1);
    assert!(new.requirements.is_empty());
    assert!(new.problems.is_empty(), "{:?}", new.problems);
}
