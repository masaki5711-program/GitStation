//! `forge explain <CODE>` and `forge errors list` — the catalog at the
//! operator's (and Claude's) fingertips during an incident.

use forge_errors::{ErrorCode, ForgeError};

/// Full explanation of one code, or `FG-SYS-0003` if it is not registered.
pub fn explain(raw: &str) -> Result<String, ForgeError> {
    let wanted = raw.trim().to_ascii_uppercase();
    let code = ErrorCode::parse(&wanted).ok_or_else(|| {
        ForgeError::new(ErrorCode::UnknownErrorCode).with_context("given", wanted.clone())
    })?;
    Ok(code.entry().render_markdown())
}

/// One line per code: id, HTTP status, severity, first sentence of the cause.
pub fn list() -> String {
    ErrorCode::all()
        .map(|c| {
            let e = c.entry();
            let cause = e.cause.split(". ").next().unwrap_or(e.cause);
            let retired = if e.retired { " (retired)" } else { "" };
            format!(
                "{}\t{}\t{}\t{}{}\n",
                e.id,
                e.http_status,
                e.severity.as_str(),
                cause,
                retired
            )
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn explain_shows_cause_diagnosis_fix_and_runbook() {
        let text = explain("fg-sys-0001").unwrap();
        assert!(text.contains("# FG-SYS-0001"));
        assert!(text.contains("調べ方 (diagnosis)"));
        assert!(text.contains("internal-error"));
    }

    #[test]
    fn explain_rejects_unknown_code() {
        let err = explain("FG-SYS-9999").unwrap_err();
        assert_eq!(err.code(), ErrorCode::UnknownErrorCode);
        assert_eq!(err.code().id(), "FG-SYS-0003");
        assert!(err.to_string().contains("given=FG-SYS-9999"));
    }

    #[test]
    fn list_has_one_line_per_code() {
        assert_eq!(list().lines().count(), ErrorCode::all().count());
    }
}
