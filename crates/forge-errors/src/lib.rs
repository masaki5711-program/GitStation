//! forge error registry.
//!
//! Every failure that can reach a log or a user carries an [`ErrorCode`]
//! (`FG-<AREA>-<NNNN>`), generated from `errors.toml`. A [`ForgeError`] pairs a
//! code with safe context fields and an optional source error; it is logged
//! once at the handling boundary and rendered to users as the localized
//! message plus the request id — never the internals.
//!
//! See `README.md` in this crate and `.claude/rules/forge-error-contract.md`.

mod catalog;
mod error;

pub use catalog::{CatalogEntry, Lang, Severity};
pub use error::ForgeError;

include!(concat!(env!("OUT_DIR"), "/generated.rs"));

impl ErrorCode {
    /// All registered codes in registry order (retired ones included).
    pub fn all() -> impl Iterator<Item = ErrorCode> {
        CATALOG.iter().map(|e| e.code)
    }

    /// Look a code up by its id (`"FG-SYS-0001"`).
    pub fn parse(id: &str) -> Option<ErrorCode> {
        CATALOG.iter().find(|e| e.id == id).map(|e| e.code)
    }

    /// The full catalog entry (cause, diagnosis, fix, runbook, ...).
    pub fn entry(self) -> &'static CatalogEntry {
        // Every variant is generated together with exactly one CATALOG row, so
        // the lookup cannot miss; `find` keeps this free of panicking indexing.
        CATALOG
            .iter()
            .find(|e| e.code == self)
            .unwrap_or(&catalog::MISSING_ENTRY)
    }

    /// The stable id, e.g. `"FG-GIT-0001"`.
    pub fn id(self) -> &'static str {
        self.entry().id
    }

    pub fn http_status(self) -> u16 {
        self.entry().http_status
    }

    pub fn severity(self) -> Severity {
        self.entry().severity
    }

    pub fn user_message(self, lang: Lang) -> &'static str {
        self.entry().user_message(lang)
    }
}

/// The generated catalog: one `<CODE>.md` page per code plus `README.md`
/// (index), as `(file name, contents)`. Written to `Git/docs/errors/` by the
/// `gen-catalog` binary; the `catalog_on_disk_matches_registry` test fails when
/// the committed pages drift from `errors.toml`.
pub fn catalog_files() -> Vec<(String, String)> {
    let entries = catalog::entries();
    let mut files: Vec<(String, String)> = entries
        .iter()
        .map(|e| (format!("{}.md", e.id), e.render_markdown()))
        .collect();
    files.push(("README.md".to_string(), catalog::render_index(&entries)));
    files
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn internal_error_is_alerting_500() {
        let err = ForgeError::new(ErrorCode::Internal).with_context("operation", "test");
        assert_eq!(err.code().id(), "FG-SYS-0001");
        assert_eq!(err.code().http_status(), 500);
        assert_eq!(err.code().severity(), Severity::Alert);
        assert!(err.code().entry().runbook.is_some());
    }

    #[test]
    fn config_invalid_is_alerting_500() {
        let err =
            ForgeError::new(ErrorCode::ConfigInvalid).with_context("config_key", "storage.root");
        assert_eq!(err.code().id(), "FG-SYS-0002");
        assert_eq!(err.code().http_status(), 500);
        assert_eq!(err.code().severity(), Severity::Alert);
    }

    #[test]
    fn every_code_round_trips_through_parse() {
        for code in ErrorCode::all() {
            assert_eq!(ErrorCode::parse(code.id()), Some(code));
        }
        assert_eq!(ErrorCode::parse("FG-SYS-9999"), None);
    }

    #[test]
    fn display_shows_code_and_context_but_user_message_does_not() {
        let err = ForgeError::new(ErrorCode::ConfigInvalid)
            .with_context("config_key", "storage.root")
            .with_source(std::io::Error::other("permission denied: /srv/forge"));
        let internal = err.to_string();
        assert!(internal.starts_with("FG-SYS-0002"), "{internal}");
        assert!(internal.contains("config_key=storage.root"), "{internal}");
        let user = err.user_message(Lang::Ja);
        assert!(!user.contains("/srv/forge"));
        assert!(!user.contains("config_key"));
    }

    #[test]
    fn catalog_on_disk_matches_registry() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/errors");
        for (name, body) in catalog_files() {
            let on_disk = std::fs::read_to_string(dir.join(&name)).unwrap_or_default();
            assert!(
                on_disk == body,
                "Git/docs/errors/{name} is stale — run `cargo run -p forge-errors --bin gen-catalog` in Git/"
            );
        }
    }

    #[test]
    fn alerting_codes_have_runbooks() {
        for code in ErrorCode::all() {
            let e = code.entry();
            if e.severity == Severity::Alert && !e.retired {
                assert!(e.runbook.is_some(), "{} is alert without runbook", e.id);
            }
        }
    }
}
