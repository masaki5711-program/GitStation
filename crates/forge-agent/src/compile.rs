//! The builder's generation (plan 16 A7, ADR-0032 Amendment A7): signed
//! part versions → the repository's managed files and
//! `.gitstation/agent.lock.json`. Pure and deterministic: the same parts
//! (in any order) give the same bytes (`compile_is_deterministic`).
//!
//! 1. **Each part is verified first** (the first of the three places of
//!    plan 16 §2.2): its manifest fields lint, its digest is recomputed
//!    from its files' contents and must be the stored one, and its
//!    signature must hold under the key ring (`FG-AGENT-0002` /
//!    `FG-AGENT-0003`). The server reads the parts from the database; a
//!    row changed there is caught here.
//! 2. **The selection**: slugs unique, exactly one `adapter` (the base
//!    model), every `requires` (`slug@range`) met by a selected part
//!    (`FG-AGENT-0008`).
//! 3. **The files**: each part's files at their install paths. Two parts
//!    may install the same path only when it is a [`MERGED_PATHS`] JSON
//!    file (`.claude/settings.json`, `.mcp.json`): the file is then the
//!    deep merge of the parts' objects in slug order ([`merge_json`]: keys
//!    united, arrays concatenated without exact duplicates, two different
//!    scalars a `merge_conflict`), and the lock carries each source's text
//!    (`content`) so every verifier — `forge/agent-lock`, `gstn agent
//!    verify` — recomputes the merge from signed bytes alone. Any other
//!    shared path is `path_conflict`, as is a hand-added `custom` file at a
//!    part's path.
//! 4. **The lock** lists the parts by slug, each with its files by path,
//!    and the `custom` files carried over from the previous lock.

use std::collections::{BTreeMap, BTreeSet};

use forge_errors::ErrorCode;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::lock::LOCK_PATH;
use crate::manifest::manifest_from_parts;
use crate::semver::{split_require, Version};
use crate::{case_key, digest, hex, sha256, verify_signature, Digest, KeyRing, Kind, Problem};

/// The JSON files several parts may write together (merged).
pub const MERGED_PATHS: [&str; 2] = [".claude/settings.json", ".mcp.json"];
/// The largest source text of a merged file (bytes).
pub const MAX_MERGE_SOURCE: usize = 64 * 1024;
/// The most parts one build selects.
pub const MAX_SELECTED_PARTS: usize = 200;
/// How deep a merged JSON value nests at most.
const MAX_MERGE_DEPTH: usize = 32;

/// One selected, published part version with its files' contents (from
/// the database).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePart {
    pub slug: String,
    pub kind: String,
    pub version: String,
    pub requires: Vec<String>,
    pub capabilities: Vec<String>,
    /// `sha256:<hex>`, as published.
    pub digest: String,
    pub key_id: String,
    pub signature: String,
    /// `(install path, content)`.
    pub files: Vec<(String, Vec<u8>)>,
    /// `rule` / `user` / `llm` (plan 09 §4).
    pub selected_by: String,
    pub reason: Option<String>,
}

/// A hand-added file kept from the previous lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CustomFile {
    pub path: String,
    pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_by: Option<String>,
}

/// What a build is for.
#[derive(Debug, Clone, Copy)]
pub struct CompileInput<'a> {
    pub project_id: i64,
    pub parts: &'a [SourcePart],
    pub custom: &'a [CustomFile],
    pub keys: &'a KeyRing,
}

/// The build's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compiled {
    /// Every file the parts install (merged ones merged), by path — the
    /// lock not included, `custom` files not included (they stay as the
    /// repository has them).
    pub files: BTreeMap<String, Vec<u8>>,
    /// The bytes of `.gitstation/agent.lock.json`.
    pub lock: Vec<u8>,
    /// `sha256:<hex>` of [`Self::lock`] (`forge.agent_builds.lock_digest`).
    pub lock_digest: String,
}

impl Compiled {
    /// Every file the build writes: the parts' and the lock.
    pub fn written(&self) -> BTreeMap<String, Vec<u8>> {
        let mut all = self.files.clone(); // clone: the caller owns its map
        all.insert(LOCK_PATH.to_owned(), self.lock.clone()); // clone: as above
        all
    }
}

fn unresolved(reason: &'static str, subject: impl Into<String>) -> Problem {
    Problem::new(ErrorCode::SelectionUnresolved, reason, Some(subject.into()))
}

#[derive(Serialize)]
struct OutLock<'a> {
    schema_version: u32,
    project_id: i64,
    parts: Vec<OutPart<'a>>,
    custom: &'a [CustomFile],
}

#[derive(Serialize)]
struct OutPart<'a> {
    slug: &'a str,
    kind: &'a str,
    version: &'a str,
    requires: Vec<String>,
    capabilities: Vec<String>,
    digest: &'a str,
    key_id: &'a str,
    signature: &'a str,
    selected_by: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
    files: Vec<OutFile>,
}

#[derive(Serialize)]
struct OutFile {
    path: String,
    sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
}

/// Verify one part's own chain (module docs, step 1).
fn verify_part(part: &SourcePart, keys: &KeyRing, problems: &mut Vec<Problem>) {
    let label = format!("{}@{}", part.slug, part.version);
    let paths: Vec<String> = part.files.iter().map(|(p, _)| p.clone()).collect();
    let manifest = match manifest_from_parts(
        &part.slug,
        &part.kind,
        &part.version,
        &part.requires,
        &part.capabilities,
        &paths,
    ) {
        Ok(m) => m,
        Err(mut found) => {
            problems.append(&mut found);
            return;
        }
    };
    let hashes: Vec<(String, [u8; 32])> = part
        .files
        .iter()
        .map(|(p, c)| (p.clone(), sha256(c)))
        .collect();
    let recomputed = digest(&manifest, &hashes);
    if Digest::parse(&part.digest) != Some(recomputed) {
        problems.push(Problem::about(
            ErrorCode::PartSignatureInvalid,
            "digest_mismatch",
            &label,
        ));
        return;
    }
    if let Err(p) = verify_signature(keys, &part.key_id, &recomputed, &part.signature) {
        let what = p.subject.unwrap_or_default();
        problems.push(Problem::new(
            p.code,
            p.reason,
            Some(format!("{label} key {what}")),
        ));
    }
}

/// The selection's own rules (module docs, step 2).
fn check_selection(parts: &[&SourcePart], problems: &mut Vec<Problem>) {
    if parts.len() > MAX_SELECTED_PARTS {
        problems.push(unresolved("too_many_parts", parts.len().to_string()));
        return;
    }
    let mut slugs: BTreeMap<&str, &SourcePart> = BTreeMap::new();
    for p in parts {
        if slugs.insert(p.slug.as_str(), p).is_some() {
            problems.push(unresolved("duplicate_part", p.slug.clone()));
        }
    }
    let adapters: Vec<&str> = parts
        .iter()
        .filter(|p| Kind::parse(&p.kind) == Some(Kind::Adapter))
        .map(|p| p.slug.as_str())
        .collect();
    match adapters.len() {
        0 => problems.push(unresolved("adapter_missing", "")),
        1 => {}
        _ => problems.push(unresolved("adapter_multiple", adapters.join(","))),
    }
    for p in parts {
        for r in &p.requires {
            let Some((slug, range)) = split_require(r) else {
                problems.push(unresolved(
                    "require_invalid",
                    format!("{} requires {r}", p.slug),
                ));
                continue;
            };
            let met = slugs
                .get(slug)
                .and_then(|dep| Version::parse(&dep.version))
                .is_some_and(|v| range.matches(&v));
            if !met {
                problems.push(unresolved(
                    "requirement_unmet",
                    format!("{} requires {r}", p.slug),
                ));
            }
        }
    }
}

/// Whether two parts may write `path` together (merged).
pub fn is_merged_path(path: &str) -> bool {
    MERGED_PATHS.contains(&path)
}

/// Build the configuration of `input` (module docs).
pub fn compile(input: &CompileInput<'_>) -> Result<Compiled, Vec<Problem>> {
    let mut problems = Vec::new();
    if input.parts.len() > MAX_SELECTED_PARTS {
        return Err(vec![unresolved(
            "too_many_parts",
            input.parts.len().to_string(),
        )]);
    }
    let mut parts: Vec<&SourcePart> = input.parts.iter().collect();
    parts.sort_by(|a, b| a.slug.cmp(&b.slug).then(a.version.cmp(&b.version)));
    for p in &parts {
        verify_part(p, input.keys, &mut problems);
    }
    check_selection(&parts, &mut problems);
    if !problems.is_empty() {
        return Err(problems);
    }

    // Step 3: every path's writers, in slug order.
    let mut by_path: BTreeMap<String, Vec<(&str, &[u8])>> = BTreeMap::new();
    let mut spelled: BTreeMap<String, String> = BTreeMap::new();
    for p in &parts {
        for (path, content) in &p.files {
            // Two spellings a file system folds into one file are one path.
            let key = case_key(path);
            match spelled.get(&key) {
                Some(first) if first != path => {
                    problems.push(unresolved("path_conflict", format!("{first} / {path}")));
                    continue;
                }
                _ => {
                    spelled.insert(key, path.clone()); // clone: the map owns its spelling
                }
            }
            by_path
                .entry(path.clone()) // clone: the map owns its key
                .or_default()
                .push((p.slug.as_str(), content.as_slice()));
        }
    }
    for c in input.custom {
        if spelled.contains_key(&case_key(&c.path)) || case_key(&c.path) == case_key(LOCK_PATH) {
            problems.push(unresolved("path_conflict", c.path.clone()));
        }
    }
    let mut files = BTreeMap::new();
    let mut merged = BTreeSet::new();
    for (path, writers) in &by_path {
        match writers.as_slice() {
            [(_, only)] => {
                files.insert(path.clone(), only.to_vec()); // clone: the output owns it
            }
            _ if is_merged_path(path) => match merge_json(path, writers) {
                Ok(bytes) => {
                    files.insert(path.clone(), bytes); // clone: as above
                    merged.insert(path.as_str());
                }
                Err(p) => problems.push(p),
            },
            _ => {
                let who: Vec<&str> = writers.iter().map(|(s, _)| *s).collect();
                problems.push(unresolved(
                    "path_conflict",
                    format!("{path} ({})", who.join(", ")),
                ));
            }
        }
    }
    if !problems.is_empty() {
        return Err(problems);
    }

    // Step 4: the lock.
    let mut custom: Vec<CustomFile> = input.custom.to_vec();
    custom.sort_by(|a, b| a.path.cmp(&b.path));
    let out_parts: Vec<OutPart<'_>> = parts
        .iter()
        .map(|p| {
            let mut requires = p.requires.clone(); // clone: sorted for the lock
            requires.sort();
            requires.dedup();
            let mut capabilities = p.capabilities.clone(); // clone: as above
            capabilities.sort();
            capabilities.dedup();
            let mut out_files: Vec<OutFile> = p
                .files
                .iter()
                .map(|(path, content)| OutFile {
                    path: path.clone(), // clone: the lock owns it
                    sha256: hex(&sha256(content)),
                    content: merged
                        .contains(path.as_str())
                        // A merged source is a JSON object (checked by the merge).
                        .then(|| String::from_utf8_lossy(content).into_owned()),
                })
                .collect();
            out_files.sort_by(|a, b| a.path.cmp(&b.path));
            OutPart {
                slug: &p.slug,
                kind: &p.kind,
                version: &p.version,
                requires,
                capabilities,
                digest: &p.digest,
                key_id: &p.key_id,
                signature: &p.signature,
                selected_by: &p.selected_by,
                reason: p.reason.as_deref(),
                files: out_files,
            }
        })
        .collect();
    let lock = OutLock {
        schema_version: 1,
        project_id: input.project_id,
        parts: out_parts,
        custom: &custom,
    };
    let mut bytes = serde_json::to_vec_pretty(&lock)
        .map_err(|_| vec![unresolved("lock_unserializable", LOCK_PATH)])?;
    bytes.push(b'\n');
    let lock_digest = format!("sha256:{}", hex(&sha256(&bytes)));
    Ok(Compiled {
        files,
        lock: bytes,
        lock_digest,
    })
}

/// Deep-merge the JSON objects `sources` (`(slug, text)`, in slug order)
/// written to `path` (module docs, step 3): pretty JSON with a final
/// newline.
pub fn merge_json(path: &str, sources: &[(&str, &[u8])]) -> Result<Vec<u8>, Problem> {
    let mut acc = Value::Object(Map::new());
    for (slug, text) in sources {
        if text.len() > MAX_MERGE_SOURCE {
            return Err(unresolved(
                "merge_source_invalid",
                format!("{path} ({slug})"),
            ));
        }
        let value: Value = serde_json::from_slice(text)
            .map_err(|_| unresolved("merge_source_invalid", format!("{path} ({slug})")))?;
        if !value.is_object() {
            return Err(unresolved(
                "merge_source_invalid",
                format!("{path} ({slug})"),
            ));
        }
        merge_into(&mut acc, value, path, "", 0)?;
    }
    let mut out = serde_json::to_vec_pretty(&acc)
        .map_err(|_| unresolved("merge_source_invalid", path.to_owned()))?;
    out.push(b'\n');
    Ok(out)
}

fn merge_into(
    acc: &mut Value,
    next: Value,
    path: &str,
    at: &str,
    depth: usize,
) -> Result<(), Problem> {
    if depth > MAX_MERGE_DEPTH {
        return Err(unresolved("merge_source_invalid", format!("{path} {at}")));
    }
    match (acc, next) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                let here = format!("{at}/{k}");
                match a.get_mut(&k) {
                    Some(existing) => merge_into(existing, v, path, &here, depth + 1)?,
                    None => {
                        a.insert(k, v);
                    }
                }
            }
            Ok(())
        }
        (Value::Array(a), Value::Array(b)) => {
            for v in b {
                if !a.contains(&v) {
                    a.push(v);
                }
            }
            Ok(())
        }
        (a, b) if *a == b => Ok(()),
        _ => Err(unresolved(
            "merge_conflict",
            format!("{path} {}", if at.is_empty() { "/" } else { at }),
        )),
    }
}
