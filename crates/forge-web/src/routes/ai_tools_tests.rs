#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The tool pack's parity (plan 19 §3.3, L5-9): every tool is a registered
//! `GET` route whose action kanflow may read acting for a person, every
//! path argument is bound, and every state name a tool answers has its
//! plain words.

use forge_auth::ActAsRule;

use super::*;
use crate::registry::Verb;
use crate::routes::merge_requests::StatusReasonName;

#[test]
fn every_tool_is_a_read_route_kanflow_may_ask_acting_for_a_person() {
    let registry = crate::app::registry();
    for tool in TOOLS {
        let def = registry
            .defs()
            .iter()
            .find(|d| d.path == tool.path && d.verb == Verb::Get)
            .unwrap_or_else(|| panic!("{}: no GET route {}", tool.name, tool.path));
        assert_eq!(
            def.action.act_as_rule(),
            ActAsRule::ReadWithoutAssertion,
            "{}: {:?} is not a read acting for a person",
            tool.name,
            def.action
        );
    }
}

#[test]
fn every_path_argument_is_bound_and_every_schema_parses() {
    let pack = pack().expect("pack");
    assert_eq!(pack.tools.len(), TOOLS.len());
    for tool in &pack.tools {
        assert!(tool.read_only);
        assert_eq!(tool.method, "GET");
        let wires: Vec<&str> = tool
            .bindings
            .iter()
            .filter(|b| b.location == ArgumentIn::Path)
            .map(|b| b.wire.as_str())
            .collect();
        let holes: Vec<&str> = tool
            .path
            .split('/')
            .filter_map(|s| s.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
            .collect();
        assert_eq!(wires, holes, "{}", tool.name);
        for b in &tool.bindings {
            assert!(
                tool.parameters["properties"].get(&b.name).is_some(),
                "{}: {}",
                tool.name,
                b.name
            );
        }
    }
}

#[test]
fn every_blocking_reason_has_its_plain_words() {
    let pack = pack().expect("pack");
    for name in StatusReasonName::ALL {
        let wire = serde_json::to_value(name).expect("name");
        let wire = wire.as_str().expect("string");
        let term = pack
            .terms
            .iter()
            .find(|t| t.kind == "block_reason" && t.name == wire)
            .unwrap_or_else(|| panic!("no term for {wire}"));
        assert!(!term.ja.is_empty() && !term.en.is_empty());
    }
}

#[test]
fn as_of_is_rfc3339_utc() {
    let at = as_of_now().expect("clock");
    assert_eq!(at.len(), 20, "{at}");
    assert!(at.ends_with('Z') && at.as_bytes()[10] == b'T', "{at}");
}
