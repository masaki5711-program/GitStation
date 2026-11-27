//! The AI asset draft (plan 17 §1.5 and P17-10, ADR-0039 Amendment
//! P17-10): the local LLM proposes a fix of a skill or a playbook from an
//! improvement request's case, as a commit on a separate branch. Pure — the
//! worker (`forge-cli`) reads git and the database, asks the gateway and
//! writes the branch.
//!
//! - [`parse_request`]: the improvement request named by the draft merge
//!   request's description (written by PFS's integration, PFS-4): the
//!   trailer lines `Improvement-Request: <id>` and `Outlet: <kind>`. Only
//!   the outlets `skill` and `playbook` get a draft (plan 17 §1.3: `agent`
//!   and `model` are protected paths, the others are not AI assets).
//! - [`changed_cases`]: the evaluation cases the merge request adds or
//!   changes (the improvement's first commit is the case, §1.4).
//!   [`without_raw_data`] keeps the synthetic ones only (§4: a case holding
//!   raw data is never sent, `forge_doc::ai_asset::case_raw_data`).
//! - [`select_assets`]: the files of the outlet's kind the model may
//!   rewrite (never a protected path, `improve_draft::is_protected_path`),
//!   those a case names first, at most [`MAX_ASSET_FILES`].
//! - [`draft_input`]: the input (`internal`: synthetic cases and the asset
//!   texts, §4), secrets redacted by [`super::prepare`].
//! - [`policy`]: no findings, at most [`MAX_EDITS`] edits of the given
//!   asset files ([`super::EditPolicy`]).
//! - [`refused_edit`]: the second gate on what is committed (the path is
//!   an editable file of the outlet's kind); the worker adds the secret
//!   scan.
//! - [`commit_message`]: the draft commit's fixed message and trailers.
//!
//! The draft decides nothing: the merge request stays a draft, nothing is
//! written to the improvement branch, and a person applies the proposal.

use forge_doc::ai_asset::{case_raw_data, is_case_path, kind_of, AssetKind};
use forge_errors::ForgeError;

use super::{
    prepare, EditPolicy, InputClass, InputLimits, InputPart, OutputPolicy, PreparedInput,
    ProposedEdit,
};
use crate::improve_draft::is_protected_path;

/// The most cases one input holds.
pub const MAX_CASE_FILES: usize = 4;
/// The largest case read (bytes); a larger one is not sent.
pub const MAX_CASE_BYTES: u64 = 16 * 1024;
/// The most asset files one input holds.
pub const MAX_ASSET_FILES: usize = 6;
/// The largest asset file read or proposed (bytes).
pub const MAX_ASSET_BYTES: usize = 24 * 1024;
/// The most files one draft changes.
pub const MAX_EDITS: usize = 3;
/// The label of the asset parts (the only ones an edit may name).
pub const ASSET_LABEL: &str = "asset";
/// The draft commit's author and committer.
pub const AUTHOR_NAME: &str = "forge-ai-draft";
pub const AUTHOR_EMAIL: &str = "ai-draft@forge.invalid";
/// The longest improvement request id taken from a description.
const MAX_REQUEST_ID: usize = 64;

/// The lever of an improvement request that a draft may fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outlet {
    Skill,
    Playbook,
}

impl Outlet {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::Playbook => "playbook",
        }
    }

    /// The asset kind its files are.
    pub fn kind(self) -> AssetKind {
        match self {
            Self::Skill => AssetKind::Skill,
            Self::Playbook => AssetKind::Playbook,
        }
    }
}

/// The improvement request a draft merge request names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftRequest {
    /// PFS's improvement request id (`[A-Za-z0-9._:-]`, at most 64).
    pub request: String,
    pub outlet: Outlet,
}

/// Why no draft is asked for (logged as `skip`, nothing else happens).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// The description names no improvement request (or a malformed one).
    NoRequest,
    /// The outlet is not a skill or a playbook (plan 17 §1.3).
    OutletNotDrafted,
    /// The merge request adds or changes no case.
    NoCase,
    /// Every case holds raw data (plan 17 §4): nothing is sent.
    CaseRawData,
    /// No editable file of the outlet's kind.
    NoAsset,
}

impl Skip {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoRequest => "no_request",
            Self::OutletNotDrafted => "outlet_not_drafted",
            Self::NoCase => "no_case",
            Self::CaseRawData => "case_raw_data",
            Self::NoAsset => "no_asset",
        }
    }
}

fn valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_REQUEST_ID
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

/// The improvement request of a draft merge request's description (module
/// docs). The first line of each key counts; keys ignore ASCII case.
pub fn parse_request(description: &str) -> Result<DraftRequest, Skip> {
    let mut request = None;
    let mut outlet = None;
    for line in description.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key.eq_ignore_ascii_case("improvement-request") && request.is_none() {
            request = Some(value.to_owned());
        } else if key.eq_ignore_ascii_case("outlet") && outlet.is_none() {
            outlet = Some(value.to_ascii_lowercase());
        }
    }
    let request = request
        .filter(|r| valid_request_id(r))
        .ok_or(Skip::NoRequest)?;
    let outlet = match outlet.as_deref() {
        Some("skill") => Outlet::Skill,
        Some("playbook") => Outlet::Playbook,
        Some(_) => return Err(Skip::OutletNotDrafted),
        None => return Err(Skip::NoRequest),
    };
    Ok(DraftRequest { request, outlet })
}

/// One file as listed: its path and blob id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub path: String,
    pub oid: String,
    pub size: u64,
}

/// The cases of `head` that `base` does not have with the same blob (new
/// or changed), in path order, at most [`MAX_CASE_FILES`], none over
/// [`MAX_CASE_BYTES`].
pub fn changed_cases<'a>(head: &'a [Listed], base: &[Listed]) -> Vec<&'a Listed> {
    let mut out: Vec<&Listed> = head
        .iter()
        .filter(|h| is_case_path(&h.path) && h.size <= MAX_CASE_BYTES)
        .filter(|h| !base.iter().any(|b| b.path == h.path && b.oid == h.oid))
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.truncate(MAX_CASE_FILES);
    out
}

/// A file's text, as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftFile {
    pub path: String,
    pub text: String,
}

/// The cases without raw data (plan 17 §4), and how many were left out.
pub fn without_raw_data(cases: Vec<DraftFile>) -> (Vec<DraftFile>, usize) {
    let before = cases.len();
    let kept: Vec<DraftFile> = cases
        .into_iter()
        .filter(|c| case_raw_data(&c.path, &c.text).is_empty())
        .collect();
    let left_out = before - kept.len();
    (kept, left_out)
}

/// Whether the draft of `outlet` may rewrite `path`.
pub fn is_editable(outlet: Outlet, path: &str) -> bool {
    kind_of(path) == Some(outlet.kind()) && !is_protected_path(path)
}

/// The asset files to send (module docs): editable, at most
/// [`MAX_ASSET_BYTES`] each; those whose file stem a case names first,
/// then in path order; at most [`MAX_ASSET_FILES`].
pub fn select_assets<'a>(
    outlet: Outlet,
    listed: &'a [Listed],
    cases: &[DraftFile],
) -> Vec<&'a Listed> {
    let named = |path: &str| {
        let stem = path
            .rsplit('/')
            .next()
            .and_then(|n| n.rsplit_once('.').map(|(s, _)| s))
            .unwrap_or("");
        !stem.is_empty() && cases.iter().any(|c| c.text.contains(stem))
    };
    let mut out: Vec<&Listed> = listed
        .iter()
        .filter(|l| {
            is_editable(outlet, &l.path)
                && usize::try_from(l.size).is_ok_and(|s| s <= MAX_ASSET_BYTES)
        })
        .collect();
    out.sort_by(|a, b| {
        named(&b.path)
            .cmp(&named(&a.path))
            .then_with(|| a.path.cmp(&b.path))
    });
    out.truncate(MAX_ASSET_FILES);
    out
}

/// The input of one draft (module docs): the request, the cases, the
/// assets. `FG-LLM-0005` when it cannot be sent.
pub fn draft_input(
    req: &DraftRequest,
    title: &str,
    cases: &[DraftFile],
    assets: &[DraftFile],
) -> Result<PreparedInput, ForgeError> {
    let title: String = title
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_TITLE_CHARS)
        .collect();
    let mut parts = vec![InputPart::new(
        "improvement_request",
        InputClass::Internal,
        format!(
            "improvement request: {}\noutlet: {}\nmerge request title: {title}",
            req.request,
            req.outlet.as_str()
        ),
    )];
    for c in cases {
        parts.push(InputPart::new("case", InputClass::Internal, c.text.clone()).with_path(&c.path));
        // clone: the part owns its text
    }
    for a in assets {
        parts.push(
            InputPart::new(ASSET_LABEL, InputClass::Internal, a.text.clone()).with_path(&a.path), // clone: as above
        );
    }
    prepare(parts, &InputLimits::default())
}

/// The output policy of a draft (module docs).
pub fn policy() -> OutputPolicy {
    OutputPolicy {
        max_findings: 0,
        max_summary_chars: 1_200,
        edits: Some(EditPolicy {
            label: ASSET_LABEL,
            max_edits: MAX_EDITS,
            max_bytes: MAX_ASSET_BYTES,
        }),
        ..OutputPolicy::default()
    }
}

/// Why an edit the output check kept is still not committed (`None`: it
/// may be). The worker adds the secret scan of the content.
///
/// Against `original` (the file's text at the head): the draft never
/// changes a number (judgement thresholds are people's, plan 17 §1.5),
/// never adds a link or a mention the file did not have (injected text
/// trying to plant one).
pub fn refused_edit(outlet: Outlet, edit: &ProposedEdit, original: &str) -> Option<&'static str> {
    if !is_editable(outlet, &edit.path) {
        return Some("not_editable");
    }
    if edit.content.len() > MAX_ASSET_BYTES {
        return Some("too_large");
    }
    if numbers(&edit.content) != numbers(original) {
        return Some("numbers_changed");
    }
    let links = |t: &str| t.matches("http://").count() + t.matches("https://").count();
    if links(&edit.content) > links(original) {
        return Some("new_link");
    }
    let mentions = mention_tokens(original);
    if mention_tokens(&edit.content)
        .iter()
        .any(|m| !mentions.contains(m))
    {
        return Some("new_mention");
    }
    None
}

/// The numbers of `text` (runs of digits with `.` inside), sorted.
fn numbers(text: &str) -> Vec<String> {
    let mut out: Vec<String> = text
        .split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map(|t| t.trim_matches('.'))
        .filter(|t| t.chars().any(|c| c.is_ascii_digit()))
        .map(str::to_owned)
        .collect();
    out.sort_unstable();
    out
}

/// The `@name` tokens of `text` (an `@` at a word start).
fn mention_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut prev: Option<char> = None;
    for (i, c) in text.char_indices() {
        if c == '@' && !prev.is_some_and(char::is_alphanumeric) {
            let name: String = text
                .get(i + 1..)
                .unwrap_or("")
                .chars()
                .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
                .collect();
            let name = name.trim_end_matches('.');
            if !name.is_empty() {
                out.push(name.to_owned());
            }
        }
        prev = Some(c);
    }
    out
}

/// The longest merge request title given to the model (one line).
pub const MAX_TITLE_CHARS: usize = 200;

/// What a draft commit's message names.
#[derive(Debug, Clone, Copy)]
pub struct CommitFacts<'a> {
    pub request: &'a DraftRequest,
    pub llm_request_id: i64,
    pub advice_id: i64,
    pub model: &'a str,
    pub prompt_version: i32,
    pub request_id: &'a str,
}

/// The draft commit's message (fixed words; the model's text is not in it).
pub fn commit_message(f: &CommitFacts<'_>) -> String {
    format!(
        "ai-draft: proposed {} fix for improvement request {}\n\n\
         A draft by the local LLM (an AI proposal). GitStation never applies it:\n\
         a person reviews it, runs the bench and applies it to the improvement\n\
         branch.\n\n\
         Improvement-Request: {}\n\
         Forge-LLM-Request: {}\n\
         Forge-LLM-Advice: {}\n\
         Forge-LLM-Model: {}\n\
         Forge-LLM-Prompt: ai_asset_draft.v{}\n\
         Forge-Request-Id: {}\n",
        f.request.outlet.as_str(),
        f.request.request,
        f.request.request,
        f.llm_request_id,
        f.advice_id,
        f.model,
        f.prompt_version,
        f.request_id,
    )
}

#[cfg(test)]
#[path = "asset_draft_tests.rs"]
mod tests;
