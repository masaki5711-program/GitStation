//! A strict YAML tree (moved from forge-core `repo_config`, plan 13 stage 2
//! MR1, ADR-0026 §8): scalars are text as written, and every YAML feature
//! that could make a file mean something else than it reads (anchors,
//! aliases, tags, a second document, duplicate keys) is refused, not
//! ignored. Only yaml-rust2's event parser is used — it expands nothing —
//! and the tree is built here with an explicit stack (bounded depth, no
//! recursion on the input).

use std::collections::HashSet;

use yaml_rust2::parser::{Event, Parser};
use yaml_rust2::scanner::{Marker, ScanError, TScalarStyle};

/// At most this many keys in one mapping (`too_many_keys`).
pub const MAX_MAP_KEYS: usize = 64;
/// The deepest nesting of collections (`too_deep`).
const MAX_DEPTH: usize = 16;

/// Why a text cannot be read: the first rule broken and its line (1-based;
/// 0 for the whole text). `reason` names the rule (`syntax`,
/// `multiple_documents`, `yaml_alias`, `yaml_anchor`, `yaml_tag`,
/// `too_deep`, `too_many_keys`, `duplicate_key`, `key_not_text`, or the
/// caller's own rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct YamlProblem {
    pub line: usize,
    pub reason: &'static str,
}

/// A problem at `line` (callers use it for their own format rules).
pub fn problem(line: usize, reason: &'static str) -> YamlProblem {
    YamlProblem { line, reason }
}

// ── the tree (scalars as text; no anchors, aliases or tags) ────────────

#[derive(Debug)]
pub struct Node {
    pub line: usize,
    pub kind: Kind,
}

#[derive(Debug)]
pub enum Kind {
    Text(String),
    /// An empty value, or a plain `~` / `null` (YAML's null): never text.
    Null,
    List(Vec<Node>),
    Map(Vec<(Node, Node)>),
}

/// A collection being filled: its line, its items, and for a mapping the
/// key waiting for its value.
enum Open {
    List(usize, Vec<Node>),
    /// Its line, its entries, the key waiting for its value, and the keys
    /// so far (duplicates are refused in every mapping, read or not).
    Map(usize, Vec<(Node, Node)>, Option<Node>, HashSet<String>),
}

fn syntax(e: &ScanError) -> YamlProblem {
    problem(line_of(e.marker()), "syntax")
}

fn line_of(mark: &Marker) -> usize {
    mark.line().max(1)
}

/// Add a finished node to the innermost open collection (`None`: it is the
/// document's root).
fn attach(stack: &mut [Open], node: Node) -> Result<Option<Node>, YamlProblem> {
    match stack.last_mut() {
        None => Ok(Some(node)),
        Some(Open::List(_, items)) => {
            items.push(node);
            Ok(None)
        }
        Some(Open::Map(_, entries, pending, keys)) => {
            match pending.take() {
                Some(key) => entries.push((key, node)),
                None => {
                    if !matches!(node.kind, Kind::Text(_)) {
                        return Err(problem(node.line, "key_not_text"));
                    }
                    // Bounded while it is built: the duplicate check and
                    // every later scan stay small.
                    if entries.len() >= MAX_MAP_KEYS {
                        return Err(problem(node.line, "too_many_keys"));
                    }
                    if let Kind::Text(name) = &node.kind {
                        if !keys.insert(name.clone()) {
                            // clone: the set owns its keys
                            return Err(problem(node.line, "duplicate_key"));
                        }
                    }
                    *pending = Some(node);
                }
            }
            Ok(None)
        }
    }
}

/// The document's root node (`None`: an empty stream). Used for
/// `.forge/repo.yml` (forge-core `repo_config`) and the spec documents'
/// front matter ([`crate::spec_doc`], M3 X4): the same refusals.
pub fn read_tree(text: &str) -> Result<Option<Node>, YamlProblem> {
    let mut parser = Parser::new_from_str(text);
    let mut stack: Vec<Open> = Vec::new();
    let mut root: Option<Node> = None;
    let mut documents = 0usize;
    loop {
        let (event, mark) = parser.next_token().map_err(|e| syntax(&e))?;
        let line = line_of(&mark);
        let finished = match event {
            Event::StreamStart | Event::Nothing | Event::DocumentEnd => None,
            Event::StreamEnd => break,
            Event::DocumentStart => {
                documents += 1;
                if documents > 1 {
                    return Err(problem(line, "multiple_documents"));
                }
                None
            }
            Event::Alias(_) => return Err(problem(line, "yaml_alias")),
            Event::Scalar(value, style, anchor, tag) => {
                refuse_extras(line, anchor, tag.is_some())?;
                let null = style == TScalarStyle::Plain
                    && matches!(value.as_str(), "" | "~" | "null" | "Null" | "NULL");
                Some(Node {
                    line,
                    kind: if null { Kind::Null } else { Kind::Text(value) },
                })
            }
            Event::SequenceStart(anchor, tag) => {
                refuse_extras(line, anchor, tag.is_some())?;
                open(&mut stack, Open::List(line, Vec::new()))?;
                None
            }
            Event::MappingStart(anchor, tag) => {
                refuse_extras(line, anchor, tag.is_some())?;
                open(
                    &mut stack,
                    Open::Map(line, Vec::new(), None, HashSet::new()),
                )?;
                None
            }
            Event::SequenceEnd | Event::MappingEnd => match stack.pop() {
                Some(Open::List(at, items)) => Some(Node {
                    line: at,
                    kind: Kind::List(items),
                }),
                Some(Open::Map(at, entries, None, _)) => Some(Node {
                    line: at,
                    kind: Kind::Map(entries),
                }),
                // A key without a value, or an end with nothing open: the
                // parser never emits these for a well-formed stream.
                Some(Open::Map(_, _, Some(_), _)) | None => return Err(problem(line, "syntax")),
            },
        };
        if let Some(node) = finished {
            if let Some(done) = attach(&mut stack, node)? {
                root = Some(done);
            }
        }
    }
    Ok(root)
}

fn refuse_extras(line: usize, anchor: usize, tagged: bool) -> Result<(), YamlProblem> {
    if anchor != 0 {
        return Err(problem(line, "yaml_anchor"));
    }
    if tagged {
        return Err(problem(line, "yaml_tag"));
    }
    Ok(())
}

fn open(stack: &mut Vec<Open>, collection: Open) -> Result<(), YamlProblem> {
    if stack.len() >= MAX_DEPTH {
        let line = match &collection {
            Open::List(l, _) | Open::Map(l, _, _, _) => *l,
        };
        return Err(problem(line, "too_deep"));
    }
    stack.push(collection);
    Ok(())
}

// ── reading a tree ──────────────────────────────────────────────────────

/// The entries of a mapping with unique text keys, or the problem.
pub fn mapping<'a>(
    node: &'a Node,
    not_a_mapping: &'static str,
) -> Result<Vec<Field<'a>>, YamlProblem> {
    let Kind::Map(entries) = &node.kind else {
        return Err(problem(node.line, not_a_mapping));
    };
    let mut out: Vec<Field<'a>> = Vec::with_capacity(entries.len());
    let mut seen: HashSet<&str> = HashSet::with_capacity(entries.len());
    for (key, value) in entries {
        let Kind::Text(name) = &key.kind else {
            return Err(problem(key.line, "key_not_text"));
        };
        if !seen.insert(name.as_str()) {
            return Err(problem(key.line, "duplicate_key"));
        }
        out.push(Field {
            key: name.as_str(),
            line: key.line,
            value,
        });
    }
    Ok(out)
}

/// One key of a mapping: its text, its line and its value.
pub struct Field<'a> {
    pub key: &'a str,
    pub line: usize,
    pub value: &'a Node,
}

/// A value that must be text: `empty` names the field's own rule for an
/// empty or null value (reported at `at`, the key's line).
pub fn text<'a>(node: &'a Node, at: usize, empty: &'static str) -> Result<&'a str, YamlProblem> {
    match &node.kind {
        Kind::Text(t) => Ok(t),
        Kind::Null => Err(problem(at, empty)),
        Kind::List(_) | Kind::Map(_) => Err(problem(node.line, "not_text")),
    }
}
