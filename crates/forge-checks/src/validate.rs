//! The rules of a check run report (M3 C1, ADR-0024 §10, docs/api/checks.md):
//! every breach is `FG-REVIEW-0020` CheckPayloadInvalid with a `reason`
//! (and `index` / `limit` / `len` where they help), never the text itself.
//! The tables' CHECKs hold the same rules as a backstop.

use forge_core::checks::{
    CheckAnnotationLevel, CheckConclusion, CheckStatus, MAX_CHECK_ANNOTATIONS,
    MAX_CHECK_DETAILS_URL_BYTES, MAX_CHECK_SUMMARY_BYTES,
};
use forge_core::CheckName;
use forge_db::checks::{CheckAnnotation, NewCheckRun};
use forge_errors::{ErrorCode, ForgeError};

/// The longest annotation path, in bytes.
pub const MAX_ANNOTATION_PATH_BYTES: usize = 4096;
/// The longest annotation message, in bytes.
pub const MAX_ANNOTATION_MESSAGE_BYTES: usize = 4096;
/// The longest annotation title, in bytes.
pub const MAX_ANNOTATION_TITLE_BYTES: usize = 255;

/// One annotation as the request sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotationInput {
    pub path: String,
    pub start_line: i64,
    pub end_line: Option<i64>,
    pub level: String,
    pub message: String,
    pub title: Option<String>,
}

/// A report as the request sent it (the name is parsed first, by
/// [`parse_name`]: it is an input of the decision).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportInput {
    pub status: String,
    pub conclusion: Option<String>,
    pub summary: Option<String>,
    pub details_url: Option<String>,
    pub annotations: Vec<AnnotationInput>,
}

pub(crate) fn invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::CheckPayloadInvalid).with_context("reason", reason)
}

/// The report's name (`FG-REVIEW-0020 reason=name_invalid`, with the
/// parser's `rule`).
pub fn parse_name(raw: &str) -> Result<CheckName, ForgeError> {
    CheckName::parse(raw).map_err(|e| {
        invalid("name_invalid")
            .with_context("rule", e.as_str())
            .with_context("len", raw.len())
    })
}

/// The report `input` for `name` under the Checks API's rules.
pub fn validate(name: CheckName, input: &ReportInput) -> Result<NewCheckRun, ForgeError> {
    let status = CheckStatus::parse(&input.status).ok_or_else(|| invalid("status_invalid"))?;
    let conclusion = input
        .conclusion
        .as_deref()
        .map(|c| CheckConclusion::parse(c).ok_or_else(|| invalid("conclusion_invalid")))
        .transpose()?;
    match (status, conclusion) {
        (CheckStatus::Completed, None) => return Err(invalid("completed_without_conclusion")),
        (CheckStatus::Queued | CheckStatus::InProgress, Some(_)) => {
            return Err(invalid("conclusion_without_completed"))
        }
        (CheckStatus::Completed, Some(_))
        | (CheckStatus::Queued | CheckStatus::InProgress, None) => {}
    }
    let summary = input.summary.clone().unwrap_or_default(); // clone: the run owns its summary
    if summary.len() > MAX_CHECK_SUMMARY_BYTES {
        return Err(invalid("summary_too_long")
            .with_context("limit", MAX_CHECK_SUMMARY_BYTES)
            .with_context("len", summary.len()));
    }
    if summary.contains('\0') {
        return Err(invalid("summary_invalid"));
    }
    let details_url = input.details_url.as_deref().map(details_url).transpose()?;
    if input.annotations.len() > MAX_CHECK_ANNOTATIONS {
        return Err(invalid("too_many_annotations")
            .with_context("limit", MAX_CHECK_ANNOTATIONS)
            .with_context("len", input.annotations.len()));
    }
    let annotations = input
        .annotations
        .iter()
        .enumerate()
        .map(|(i, a)| annotation(a).map_err(|e| e.with_context("index", i)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(NewCheckRun {
        name,
        status,
        conclusion,
        summary,
        details_url,
        annotations,
    })
}

/// An http(s) URL with a host and no credentials, at most 2 KiB once
/// normalized (the stored form). The server never fetches it.
fn details_url(raw: &str) -> Result<String, ForgeError> {
    let bad = || invalid("details_url_invalid");
    if raw.len() > MAX_CHECK_DETAILS_URL_BYTES {
        return Err(bad().with_context("limit", MAX_CHECK_DETAILS_URL_BYTES));
    }
    if raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(bad());
    }
    let url = url::Url::parse(raw).map_err(|_| bad())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none_or(str::is_empty) {
        return Err(bad());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(bad().with_context("rule", "credentials"));
    }
    let stored = url.as_str().to_owned();
    if stored.len() > MAX_CHECK_DETAILS_URL_BYTES {
        return Err(bad().with_context("limit", MAX_CHECK_DETAILS_URL_BYTES));
    }
    Ok(stored)
}

/// A path inside the repository: relative (no leading `/`, no drive letter
/// `C:` — review fix 8), no `..` segment, no backslash, no control
/// character, 1–4096 bytes.
pub(crate) fn relative_path(path: &str) -> bool {
    let drive = matches!(path.as_bytes(), [letter, b':', ..] if letter.is_ascii_alphabetic());
    !path.is_empty()
        && path.len() <= MAX_ANNOTATION_PATH_BYTES
        && !path.starts_with('/')
        && !drive
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && !path.split('/').any(|segment| segment == "..")
}

fn annotation(a: &AnnotationInput) -> Result<CheckAnnotation, ForgeError> {
    if !relative_path(&a.path) {
        return Err(invalid("annotation_path_invalid").with_context("len", a.path.len()));
    }
    let lines = || invalid("annotation_lines_invalid");
    let start = u32::try_from(a.start_line)
        .ok()
        .filter(|&l| l >= 1 && l <= i32::MAX.unsigned_abs())
        .ok_or_else(lines)?;
    let end = match a.end_line {
        None => start,
        Some(end) => u32::try_from(end)
            .ok()
            .filter(|&l| l >= start && l <= i32::MAX.unsigned_abs())
            .ok_or_else(lines)?,
    };
    let level =
        CheckAnnotationLevel::parse(&a.level).ok_or_else(|| invalid("annotation_level_invalid"))?;
    if a.message.is_empty()
        || a.message.len() > MAX_ANNOTATION_MESSAGE_BYTES
        || a.message.contains('\0')
    {
        return Err(invalid("annotation_message_invalid")
            .with_context("limit", MAX_ANNOTATION_MESSAGE_BYTES)
            .with_context("len", a.message.len()));
    }
    if let Some(title) = &a.title {
        if title.len() > MAX_ANNOTATION_TITLE_BYTES || title.contains('\0') {
            return Err(invalid("annotation_title_invalid")
                .with_context("limit", MAX_ANNOTATION_TITLE_BYTES)
                .with_context("len", title.len()));
        }
    }
    Ok(CheckAnnotation {
        path: a.path.clone(), // clone: the run owns its annotations
        start_line: start,
        end_line: end,
        level,
        message: a.message.clone(), // clone: as above
        title: a.title.clone(),     // clone: as above
    })
}

#[cfg(test)]
#[path = "validate_tests.rs"]
mod tests;
