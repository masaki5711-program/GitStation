//! Document types: `.forge/doctypes/<name>.yml` (plan 13 §5, ADR-0026 §5).
//!
//! A definition says what a document of the type must hold — its extra
//! front matter fields, its headings (in order), its blocks and their
//! attributes, the id patterns, and the deterministic rules the lint runs.
//! It never holds governance (who approves, the phases, how concurrent
//! editing is relaxed, who may lower a classification): those live in the
//! repository settings (ADR-0027, ADR-0028). `approval_hint` and
//! `concurrency_hint` are suggestions only and are read for their shape.
//!
//! [`parse_doctype`] is pure: bytes in, a [`Doctype`] or the first
//! [`DoctypeProblem`] out. Every key is known or refused (an unknown key is
//! a typo that would otherwise silently weaken a check).

use regex::{Regex, RegexBuilder};

use crate::yaml::{mapping, problem, read_tree, text, Field, Kind, Node, YamlProblem};

/// A larger definition is refused (`too_large`).
pub const MAX_DOCTYPE_BYTES: usize = 64 * 1024;
/// The longest `id_pattern` (`pattern_invalid`).
pub const MAX_PATTERN_LEN: usize = 256;
/// The compiled size limit of one pattern (`pattern_invalid` beyond it).
const PATTERN_SIZE_LIMIT: usize = 64 * 1024;
/// At most this many fields, sections, block kinds, attributes per block,
/// rules or enum values (`too_many`).
pub const MAX_ITEMS: usize = 64;

/// Why a definition cannot be used: the first rule broken and its line
/// (1-based; 0 for the whole file).
pub type DoctypeProblem = YamlProblem;

/// A value's type: a front matter field's or a block attribute's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueType {
    /// Any text.
    Text,
    /// A list of texts (front matter fields only).
    List,
    /// A decimal integer, optionally bounded (inclusive).
    Int { min: Option<i64>, max: Option<i64> },
    /// `true` / `false`.
    Bool,
    /// One of the listed words.
    Enum(Vec<String>),
    /// `<number><D|W|M|Y>` (`24M`, `5Y`; plan 13 §14 decision 4).
    Duration,
    /// Another document's id, optionally of one type (`ref` in a field).
    DocRef { doctype: Option<String> },
    /// A block of the same document, optionally of one kind.
    BlockRef { kind: Option<String> },
}

/// A front matter field or a block attribute the type declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueDef {
    pub key: String,
    pub ty: ValueType,
    pub required: bool,
}

/// A required heading (a level-2 heading with exactly this text) and the
/// block kinds that belong under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionDef {
    pub title: String,
    pub blocks: Vec<String>,
}

/// A block kind the type allows.
#[derive(Debug, Clone)]
pub struct BlockDef {
    pub kind: String,
    /// The id's shape; `None`: `req`'s requirement id, any other kind the
    /// default ([`DEFAULT_BLOCK_ID`]).
    pub id_pattern: Option<Pattern>,
    pub attrs: Vec<ValueDef>,
}

/// The default block id: a letter or digit, then letters, digits, `.` and
/// `-`, at most 64 characters (ADR-0026 §2).
pub const DEFAULT_BLOCK_ID: &str = "^[A-Za-z0-9][A-Za-z0-9.-]{0,63}$";

/// A compiled `id_pattern`: the whole id must match.
#[derive(Debug, Clone)]
pub struct Pattern {
    source: String,
    re: Regex,
}

impl Pattern {
    /// `None` when `source` is too long, not a valid regular expression or
    /// too large compiled.
    pub fn new(source: &str) -> Option<Self> {
        if source.is_empty() || source.len() > MAX_PATTERN_LEN {
            return None;
        }
        // The source must stand on its own: `a)|(b` would otherwise close the
        // group around it and escape the anchors (security review MR2a).
        RegexBuilder::new(source)
            .size_limit(PATTERN_SIZE_LIMIT)
            .dfa_size_limit(PATTERN_SIZE_LIMIT)
            .build()
            .ok()?;
        let re = RegexBuilder::new(&format!(r"\A(?:{source})\z"))
            .size_limit(PATTERN_SIZE_LIMIT)
            .dfa_size_limit(PATTERN_SIZE_LIMIT)
            .build()
            .ok()?;
        Some(Self {
            source: source.to_owned(),
            re,
        })
    }

    pub fn matches(&self, id: &str) -> bool {
        self.re.is_match(id)
    }

    pub fn as_str(&self) -> &str {
        &self.source
    }
}

/// A rule the lint runs for documents of the type (deterministic only;
/// plan 13 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// A `decision` with `required=true` is not `state=open` (W12's gate).
    DecisionsResolved,
    /// A `decision` with `state=na` says why in its body.
    DecisionNaNeedsReason,
    /// Every `req` with `priority=must` is covered by a `check`.
    EveryMustReqHasCheck,
    /// Every `record` names its form (`form=`).
    EveryRecordNamesAForm,
    /// Every `hazard` has a `control` (`for=`).
    EveryHazardHasControl,
    /// `res_severity` / `res_likelihood` never exceed the initial values.
    ResidualNotAboveInitial,
    /// A controlled hazard states its residual risk (`res_*`).
    ResidualSetWhenControlled,
}

impl Rule {
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "decisions_resolved" => Self::DecisionsResolved,
            "decision_na_needs_reason" => Self::DecisionNaNeedsReason,
            "every_must_req_has_check" => Self::EveryMustReqHasCheck,
            "every_record_names_a_form" => Self::EveryRecordNamesAForm,
            "every_hazard_has_control" => Self::EveryHazardHasControl,
            "residual_not_above_initial" => Self::ResidualNotAboveInitial,
            "residual_set_when_controlled" => Self::ResidualSetWhenControlled,
            _ => return None,
        })
    }

    /// The rule's name: also the lint problem's rule.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DecisionsResolved => "decisions_resolved",
            Self::DecisionNaNeedsReason => "decision_na_needs_reason",
            Self::EveryMustReqHasCheck => "every_must_req_has_check",
            Self::EveryRecordNamesAForm => "every_record_names_a_form",
            Self::EveryHazardHasControl => "every_hazard_has_control",
            Self::ResidualNotAboveInitial => "residual_not_above_initial",
            Self::ResidualSetWhenControlled => "residual_set_when_controlled",
        }
    }
}

/// A parsed definition (module docs).
#[derive(Debug, Clone)]
pub struct Doctype {
    pub name: String,
    pub version: Option<u32>,
    /// `(language, label)` in file order.
    pub label: Vec<(String, String)>,
    /// The document id's shape (`None`: any id the default allows).
    pub id_pattern: Option<Pattern>,
    /// `defaults.classification` (`None`: `internal`).
    pub default_classification: Option<crate::document::Classification>,
    pub default_review_cycle: Option<String>,
    pub fields: Vec<ValueDef>,
    pub sections: Vec<SectionDef>,
    pub blocks: Vec<BlockDef>,
    pub rules: Vec<Rule>,
}

impl Doctype {
    pub fn block(&self, kind: &str) -> Option<&BlockDef> {
        self.blocks.iter().find(|b| b.kind == kind)
    }

    pub fn field(&self, key: &str) -> Option<&ValueDef> {
        self.fields.iter().find(|f| f.key == key)
    }

    pub fn has_rule(&self, rule: Rule) -> bool {
        self.rules.contains(&rule)
    }
}

/// The front matter keys every document may have (ADR-0026 §1): a type
/// cannot redeclare them as its own fields (review MR2a: the base would
/// silently win).
pub const BASE_FIELDS: &[&str] = &[
    "doctype",
    "id",
    "title",
    "owner",
    "classification",
    "review_cycle",
    "related",
    "lang",
    "translation_of",
    "retired",
    // The older mark (`true` / `false`), read as a classification.
    "confidential",
];

/// Attribute keys every block with an id has (ADR-0026 §2, plan 13 §14
/// decision 6): a type cannot redeclare them.
pub const RESERVED_ATTRS: &[&str] = &["status", "replaced_by"];

/// Whether `name` is a type's name: dot-separated words of lower-case
/// letters, digits and `-`, each starting with a letter.
pub fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.split('.').all(|part| {
            part.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// Whether `raw` is a block kind (`[a-z][a-z0-9-]*`) or an attribute key
/// (`[a-z][a-z0-9_]*`, with `underscore`).
pub(crate) fn word_ok(raw: &str, underscore: bool) -> bool {
    raw.len() <= 64
        && raw.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && raw.bytes().all(|b| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || if underscore { b == b'_' } else { b == b'-' }
        })
}

/// Whether `raw` is a duration: 1–4 digits then `D`, `W`, `M` or `Y`.
pub fn duration_ok(raw: &str) -> bool {
    let Some((digits, unit)) = raw.split_at_checked(raw.len().saturating_sub(1)) else {
        return false;
    };
    (1..=4).contains(&digits.len())
        && digits.bytes().all(|b| b.is_ascii_digit())
        && matches!(unit, "D" | "W" | "M" | "Y")
}

/// Parse `.forge/doctypes/<stem>.yml` (module docs): `name` must equal the
/// file's stem.
pub fn parse_doctype(stem: &str, bytes: &[u8]) -> Result<Doctype, DoctypeProblem> {
    if bytes.len() > MAX_DOCTYPE_BYTES {
        return Err(problem(0, "too_large"));
    }
    let text_in = std::str::from_utf8(bytes).map_err(|_| problem(0, "not_utf8"))?;
    let Some(root) = read_tree(text_in)? else {
        return Err(problem(0, "empty"));
    };
    let mut out = Doctype {
        name: String::new(),
        version: None,
        label: Vec::new(),
        id_pattern: None,
        default_classification: None,
        default_review_cycle: None,
        fields: Vec::new(),
        sections: Vec::new(),
        blocks: Vec::new(),
        rules: Vec::new(),
    };
    let mut name_line = None;
    for field in mapping(&root, "not_a_mapping")? {
        match field.key {
            "name" => {
                out.name = text(field.value, field.line, "name_invalid")?.to_owned();
                name_line = Some(field.line);
            }
            "version" => {
                let raw = text(field.value, field.line, "version_invalid")?;
                out.version = Some(
                    raw.parse::<u32>()
                        .map_err(|_| problem(field.line, "version_invalid"))?,
                );
            }
            "label" => out.label = labels(&field)?,
            "id_pattern" => {
                let raw = text(field.value, field.line, "pattern_invalid")?;
                out.id_pattern =
                    Some(Pattern::new(raw).ok_or_else(|| problem(field.line, "pattern_invalid"))?);
            }
            "defaults" => defaults(&field, &mut out)?,
            "fields" => out.fields = value_defs(field.value, "fields_invalid", false)?,
            "sections" => out.sections = sections(field.value)?,
            "blocks" => out.blocks = blocks(field.value)?,
            "rules" => out.rules = rules(field.value)?,
            // Suggestions only (governance is the repository's settings):
            // read for their shape, never used to decide.
            "approval_hint" => hint_map(field.value)?,
            "concurrency_hint" => {
                let raw = text(field.value, field.line, "hint_invalid")?;
                if !matches!(raw, "single_revision" | "free") {
                    return Err(problem(field.line, "hint_invalid"));
                }
            }
            "export" => {
                let raw = text(field.value, field.line, "export_invalid")?;
                if !name_ok(raw) {
                    return Err(problem(field.line, "export_invalid"));
                }
            }
            _ => return Err(problem(field.line, "unknown_key")),
        }
    }
    let line = name_line.ok_or_else(|| problem(0, "name_missing"))?;
    if !name_ok(&out.name) {
        return Err(problem(line, "name_invalid"));
    }
    if out.name != stem {
        return Err(problem(line, "name_not_filename"));
    }
    // A section names a kind the type does not define.
    let unknown = out
        .sections
        .iter()
        .any(|s| s.blocks.iter().any(|k| out.block(k).is_none()));
    if unknown {
        return Err(problem(0, "section_block_unknown"));
    }
    Ok(out)
}

fn labels(field: &Field<'_>) -> Result<Vec<(String, String)>, DoctypeProblem> {
    let entries = mapping(field.value, "label_invalid")?;
    if entries.len() > MAX_ITEMS {
        return Err(problem(field.line, "too_many"));
    }
    entries
        .iter()
        .map(|e| {
            let label = text(e.value, e.line, "label_invalid")?;
            Ok((e.key.to_owned(), label.to_owned()))
        })
        .collect()
}

fn defaults(field: &Field<'_>, out: &mut Doctype) -> Result<(), DoctypeProblem> {
    for e in mapping(field.value, "defaults_invalid")? {
        let raw = text(e.value, e.line, "defaults_invalid")?;
        match e.key {
            "classification" => {
                out.default_classification = Some(
                    crate::document::Classification::parse(raw)
                        .ok_or_else(|| problem(e.line, "defaults_invalid"))?,
                );
            }
            "review_cycle" => {
                if !duration_ok(raw) {
                    return Err(problem(e.line, "defaults_invalid"));
                }
                out.default_review_cycle = Some(raw.to_owned());
            }
            _ => return Err(problem(e.line, "defaults_invalid")),
        }
    }
    Ok(())
}

/// A bool written in a definition (`true` / `false`).
fn bool_of(node: &Node, at: usize, reason: &'static str) -> Result<bool, DoctypeProblem> {
    match text(node, at, reason)? {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(problem(at, reason)),
    }
}

fn int_of(node: &Node, at: usize, reason: &'static str) -> Result<i64, DoctypeProblem> {
    text(node, at, reason)?
        .parse::<i64>()
        .map_err(|_| problem(at, reason))
}

/// The words of a list (`[a, b]`).
fn words(node: &Node, at: usize, reason: &'static str) -> Result<Vec<String>, DoctypeProblem> {
    let Kind::List(items) = &node.kind else {
        return Err(problem(at, reason));
    };
    if items.len() > MAX_ITEMS {
        return Err(problem(at, "too_many"));
    }
    items
        .iter()
        .map(|i| text(i, i.line, reason).map(str::to_owned))
        .collect()
}

/// `fields:` or a block's `attrs:` — `{ key: { type, required, … } }`.
fn value_defs(
    node: &Node,
    reason: &'static str,
    attrs: bool,
) -> Result<Vec<ValueDef>, DoctypeProblem> {
    let entries = mapping(node, reason)?;
    if entries.len() > MAX_ITEMS {
        return Err(problem(node.line, "too_many"));
    }
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        if !word_ok(e.key, true) {
            return Err(problem(e.line, reason));
        }
        if attrs && RESERVED_ATTRS.contains(&e.key) {
            return Err(problem(e.line, "attr_reserved"));
        }
        if !attrs && BASE_FIELDS.contains(&e.key) {
            return Err(problem(e.line, "field_reserved"));
        }
        out.push(value_def(&e, reason, attrs)?);
    }
    Ok(out)
}

fn value_def(e: &Field<'_>, reason: &'static str, attr: bool) -> Result<ValueDef, DoctypeProblem> {
    let mut ty_name: Option<(&str, usize)> = None;
    let mut required = false;
    let (mut min, mut max, mut doctype, mut kind, mut choices) = (None, None, None, None, None);
    for p in mapping(e.value, reason)? {
        match p.key {
            "type" => ty_name = Some((text(p.value, p.line, reason)?, p.line)),
            "required" => required = bool_of(p.value, p.line, reason)?,
            "label" => {
                labels(&p)?;
            }
            "min" => min = Some(int_of(p.value, p.line, reason)?),
            "max" => max = Some(int_of(p.value, p.line, reason)?),
            "doctype" => {
                let raw = text(p.value, p.line, reason)?;
                if !name_ok(raw) {
                    return Err(problem(p.line, reason));
                }
                doctype = Some(raw.to_owned());
            }
            "kind" => {
                let raw = text(p.value, p.line, reason)?;
                if !word_ok(raw, false) {
                    return Err(problem(p.line, reason));
                }
                kind = Some(raw.to_owned());
            }
            "enum" => choices = Some(words(p.value, p.line, reason)?),
            _ => return Err(problem(p.line, reason)),
        }
    }
    let ty = match (ty_name, choices) {
        (None, Some(c)) | (Some(("enum", _)), Some(c)) if !c.is_empty() => ValueType::Enum(c),
        (Some(("text", _)), None) => ValueType::Text,
        (Some(("list", _)), None) if !attr => ValueType::List,
        (Some(("int", _)), None) => ValueType::Int { min, max },
        (Some(("bool", _)), None) => ValueType::Bool,
        (Some(("duration", _)), None) => ValueType::Duration,
        (Some(("ref" | "docref", _)), None) => ValueType::DocRef {
            doctype: doctype.clone(),
        },
        (Some(("blockref", _)), None) if attr => ValueType::BlockRef { kind: kind.clone() },
        (Some((_, line)), _) => return Err(problem(line, "type_unknown")),
        (None, _) => return Err(problem(e.line, "type_unknown")),
    };
    // Options that do not belong to the type are refused, not ignored.
    let stray = (min.is_some() || max.is_some()) && !matches!(ty, ValueType::Int { .. })
        || doctype.is_some() && !matches!(ty, ValueType::DocRef { .. })
        || kind.is_some() && !matches!(ty, ValueType::BlockRef { .. });
    if stray || min.zip(max).is_some_and(|(a, b)| a > b) {
        return Err(problem(e.line, reason));
    }
    Ok(ValueDef {
        key: e.key.to_owned(),
        ty,
        required,
    })
}

fn sections(node: &Node) -> Result<Vec<SectionDef>, DoctypeProblem> {
    let Kind::List(items) = &node.kind else {
        return Err(problem(node.line, "sections_invalid"));
    };
    if items.len() > MAX_ITEMS {
        return Err(problem(node.line, "too_many"));
    }
    let mut out: Vec<SectionDef> = Vec::with_capacity(items.len());
    for item in items {
        let mut title = None;
        let mut kinds = Vec::new();
        for p in mapping(item, "sections_invalid")? {
            match p.key {
                "title" => title = Some(text(p.value, p.line, "sections_invalid")?.to_owned()),
                "blocks" => kinds = words(p.value, p.line, "sections_invalid")?,
                _ => return Err(problem(p.line, "sections_invalid")),
            }
        }
        let title = title.ok_or_else(|| problem(item.line, "sections_invalid"))?;
        if out.iter().any(|s| s.title == title) {
            return Err(problem(item.line, "section_duplicate"));
        }
        out.push(SectionDef {
            title,
            blocks: kinds,
        });
    }
    Ok(out)
}

fn blocks(node: &Node) -> Result<Vec<BlockDef>, DoctypeProblem> {
    let entries = mapping(node, "blocks_invalid")?;
    if entries.len() > MAX_ITEMS {
        return Err(problem(node.line, "too_many"));
    }
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        if !word_ok(e.key, false) {
            return Err(problem(e.line, "blocks_invalid"));
        }
        let mut def = BlockDef {
            kind: e.key.to_owned(),
            id_pattern: None,
            attrs: Vec::new(),
        };
        // `req:` with nothing under it is the requirement kind as-is.
        if !matches!(e.value.kind, Kind::Null) {
            for p in mapping(e.value, "blocks_invalid")? {
                match p.key {
                    "id_pattern" => {
                        let raw = text(p.value, p.line, "pattern_invalid")?;
                        def.id_pattern = Some(
                            Pattern::new(raw).ok_or_else(|| problem(p.line, "pattern_invalid"))?,
                        );
                    }
                    "attrs" => def.attrs = value_defs(p.value, "attrs_invalid", true)?,
                    _ => return Err(problem(p.line, "blocks_invalid")),
                }
            }
        }
        out.push(def);
    }
    Ok(out)
}

fn rules(node: &Node) -> Result<Vec<Rule>, DoctypeProblem> {
    let mut out = Vec::new();
    for (i, raw) in words(node, node.line, "rules_invalid")?.iter().enumerate() {
        let line = match &node.kind {
            Kind::List(items) => items.get(i).map_or(node.line, |n| n.line),
            Kind::Text(_) | Kind::Null | Kind::Map(_) => node.line,
        };
        let rule = Rule::parse(raw).ok_or_else(|| problem(line, "rule_unknown"))?;
        if !out.contains(&rule) {
            out.push(rule);
        }
    }
    Ok(out)
}

/// `approval_hint: { review: [groups], approve: [groups] }`: shape only.
fn hint_map(node: &Node) -> Result<(), DoctypeProblem> {
    for p in mapping(node, "hint_invalid")? {
        if !word_ok(p.key, true) {
            return Err(problem(p.line, "hint_invalid"));
        }
        words(p.value, p.line, "hint_invalid")?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "doctype_tests.rs"]
mod tests;
