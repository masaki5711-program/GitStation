//! The rules-only recommendation (plan 16 §1.3 and A7, ADR-0032 Amendment
//! A7): deterministic, no model. The local LLM's reasons come with A11.
//!
//! - The base model: the adapter the current lock uses, else
//!   [`DEFAULT_ADAPTER`] (`reason=base_model`).
//! - The parts the current lock uses (`reason=current_lock`): a rebuild
//!   keeps the selection (plan 09 §4); a part with no stable version left
//!   is listed in [`Recommendation::unavailable`], not silently dropped.
//! - Everything those require, transitively, at the newest stable version
//!   in the range (`reason=required_by=<slug>`).
//!
//! Each pick is the newest published stable version (the caller passes
//! only those). The person decides: the answer is a proposal the screen
//! shows as checkboxes.

use std::collections::{BTreeMap, VecDeque};

use forge_errors::ErrorCode;

use crate::semver::{split_require, Range, Version};
use crate::{Kind, Problem};

/// The base model when the repository has none yet (plan 09 §3.2).
pub const DEFAULT_ADAPTER: &str = "claude-code-v2";

/// A published stable version the recommendation may pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogVersion {
    pub slug: String,
    pub kind: String,
    pub version: String,
    pub requires: Vec<String>,
}

/// One recommended part version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recommended {
    pub slug: String,
    pub kind: String,
    pub version: String,
    /// Always `rule` (A7).
    pub selected_by: &'static str,
    pub reason: String,
}

/// The recommendation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recommendation {
    /// By slug.
    pub parts: Vec<Recommended>,
    /// Slugs of the current lock with no stable version to pick.
    pub unavailable: Vec<String>,
}

fn unresolved(reason: &'static str, subject: impl Into<String>) -> Problem {
    Problem::new(ErrorCode::SelectionUnresolved, reason, Some(subject.into()))
}

/// The newest version of `slug` in `range` (releases only, unless the range
/// names a pre-release exactly).
fn newest<'c>(
    catalog: &'c [CatalogVersion],
    slug: &str,
    range: &Range,
) -> Option<&'c CatalogVersion> {
    catalog
        .iter()
        .filter(|c| c.slug == slug)
        .filter_map(|c| Version::parse(&c.version).map(|v| (v, c)))
        .filter(|(v, _)| range.matches(v))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, c)| c)
}

/// Recommend a selection (module docs). `adapter`: the base model's slug;
/// `current`: the slugs of the current lock.
pub fn recommend(
    catalog: &[CatalogVersion],
    adapter: &str,
    current: &[String],
) -> Result<Recommendation, Vec<Problem>> {
    let any = Range::parse("*").ok_or_else(|| vec![unresolved("require_invalid", "*")])?;
    let mut picked: BTreeMap<String, Recommended> = BTreeMap::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut problems = Vec::new();
    let mut unavailable = Vec::new();

    match newest(catalog, adapter, &any) {
        Some(c) if Kind::parse(&c.kind) == Some(Kind::Adapter) => {
            picked.insert(
                c.slug.clone(), // clone: the map owns its key
                Recommended {
                    slug: c.slug.clone(),       // clone: the answer owns it
                    kind: c.kind.clone(),       // clone: as above
                    version: c.version.clone(), // clone: as above
                    selected_by: "rule",
                    reason: "base_model".to_owned(),
                },
            );
            queue.push_back(c.slug.clone()); // clone: the queue owns it
        }
        _ => return Err(vec![unresolved("adapter_missing", adapter)]),
    }
    for slug in current {
        if picked.contains_key(slug) {
            continue;
        }
        let Some(c) = newest(catalog, slug, &any) else {
            unavailable.push(slug.clone()); // clone: the answer owns it
            continue;
        };
        // One base model: the current lock's other adapters are not kept.
        if Kind::parse(&c.kind) == Some(Kind::Adapter) {
            continue;
        }
        picked.insert(
            slug.clone(), // clone: the map owns its key
            Recommended {
                slug: slug.clone(),         // clone: the answer owns it
                kind: c.kind.clone(),       // clone: as above
                version: c.version.clone(), // clone: as above
                selected_by: "rule",
                reason: "current_lock".to_owned(),
            },
        );
        queue.push_back(slug.clone()); // clone: the queue owns it
    }
    while let Some(slug) = queue.pop_front() {
        let Some(entry) = catalog
            .iter()
            .find(|c| c.slug == slug && picked.get(&slug).is_some_and(|p| p.version == c.version))
        else {
            continue;
        };
        for r in &entry.requires {
            let Some((dep, range)) = split_require(r) else {
                problems.push(unresolved(
                    "require_invalid",
                    format!("{slug} requires {r}"),
                ));
                continue;
            };
            if let Some(have) = picked.get(dep) {
                let ok = Version::parse(&have.version).is_some_and(|v| range.matches(&v));
                if !ok {
                    problems.push(unresolved(
                        "requirement_unmet",
                        format!("{slug} requires {r}"),
                    ));
                }
                continue;
            }
            match newest(catalog, dep, &range) {
                Some(c) if Kind::parse(&c.kind) != Some(Kind::Adapter) => {
                    picked.insert(
                        dep.to_owned(),
                        Recommended {
                            slug: dep.to_owned(),
                            kind: c.kind.clone(), // clone: the answer owns it
                            version: c.version.clone(), // clone: as above
                            selected_by: "rule",
                            reason: format!("required_by={slug}"),
                        },
                    );
                    queue.push_back(dep.to_owned());
                }
                _ => problems.push(unresolved(
                    "requirement_unmet",
                    format!("{slug} requires {r}"),
                )),
            }
        }
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    Ok(Recommendation {
        parts: picked.into_values().collect(),
        unavailable,
    })
}
