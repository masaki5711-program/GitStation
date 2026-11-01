//! The web UI the shell and `/-/assets/{file}` serve (ADR-0022 §2).
//!
//! - With the feature `embed-ui`: `web/dist` inside the binary (rust-embed,
//!   embedded in every profile). `build.rs` refuses a missing or malformed
//!   `dist` (no `index.html`, a sub-directory, an extension outside the
//!   allow-list). cargo never runs npm.
//! - Without it (development, nextest, `cargo build --workspace`): the
//!   placeholder shell and no assets; `forge doctor` warns
//!   `web.ui=placeholder` ([`status`]).
//!
//! Every lookup is by the exact file name: no path joining, no disk access
//! at run time.

use std::borrow::Cow;

/// What the binary serves as the UI (for `forge doctor`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiStatus {
    /// Built without `embed-ui`: the placeholder shell, no assets.
    Placeholder,
    /// `web/dist` is embedded; the first 12 hex digits of the SHA-256 of
    /// its `index.html` tell one build from another.
    Embedded { index_sha256: String },
}

/// The UI this binary serves.
pub fn status() -> UiStatus {
    imp::status()
}

/// The shell document: the same bytes for every path (ADR-0015).
pub(crate) fn shell_html() -> &'static [u8] {
    imp::SHELL
}

/// One embedded asset and its precompressed variants.
pub(crate) struct Asset {
    pub content_type: &'static str,
    pub identity: Cow<'static, [u8]>,
    pub br: Option<Cow<'static, [u8]>>,
    pub gzip: Option<Cow<'static, [u8]>>,
}

/// The asset named `name` (one URL segment, already decoded), `None` when
/// the build has no such file. `index.html` and the `.br` / `.gz` files are
/// not assets of their own.
pub(crate) fn asset(name: &str) -> Option<Asset> {
    if name == "index.html" {
        return None;
    }
    let content_type = content_type(name)?;
    imp::asset(name, content_type)
}

/// The served extensions (ADR-0022 §2; `build.rs` refuses any other).
pub(crate) const EXTENSIONS: &[(&str, &str)] = &[
    ("js", "text/javascript; charset=utf-8"),
    ("css", "text/css; charset=utf-8"),
    ("woff2", "font/woff2"),
    ("svg", "image/svg+xml"),
    ("png", "image/png"),
    ("ico", "image/x-icon"),
    ("json", "application/json"),
    ("txt", "text/plain; charset=utf-8"),
];

fn content_type(name: &str) -> Option<&'static str> {
    let (_, ext) = name.rsplit_once('.')?;
    EXTENSIONS
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, content_type)| *content_type)
}

/// The placeholder shell (no build embedded).
#[cfg_attr(feature = "embed-ui", allow(dead_code))]
pub(crate) const PLACEHOLDER_HTML: &str = "<!doctype html>\n<html lang=\"ja\">\n<head>\n<meta charset=\"utf-8\">\n\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>GitStation</title>\n\
</head>\n<body>\n<div id=\"app\"></div>\n</body>\n</html>\n";

#[cfg(not(feature = "embed-ui"))]
mod imp {
    use super::{Asset, UiStatus};

    pub(super) const SHELL: &[u8] = super::PLACEHOLDER_HTML.as_bytes();

    pub(super) fn status() -> UiStatus {
        UiStatus::Placeholder
    }

    pub(super) fn asset(_name: &str, _content_type: &'static str) -> Option<Asset> {
        None
    }
}

#[cfg(feature = "embed-ui")]
mod imp {
    use rust_embed::RustEmbed;
    use sha2::{Digest, Sha256};

    use super::{Asset, UiStatus};

    /// A compile error, not an empty UI, when the build is missing
    /// (build.rs says why first).
    pub(super) const SHELL: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../web/dist/index.html"
    ));

    #[derive(RustEmbed)]
    // Relative to this crate (rust-embed resolves it from CARGO_MANIFEST_DIR).
    #[folder = "../../web/dist"]
    struct Dist;

    pub(super) fn status() -> UiStatus {
        let digest = Sha256::digest(SHELL);
        let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
        UiStatus::Embedded { index_sha256: hex }
    }

    pub(super) fn asset(name: &str, content_type: &'static str) -> Option<Asset> {
        if name.ends_with(".br") || name.ends_with(".gz") {
            return None;
        }
        let identity = Dist::get(name)?.data;
        Some(Asset {
            content_type,
            identity,
            br: Dist::get(&format!("{name}.br")).map(|f| f.data),
            gzip: Dist::get(&format!("{name}.gz")).map(|f| f.data),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_allow_listed_extensions_have_a_content_type() {
        assert_eq!(
            content_type("index-a1.js"),
            Some("text/javascript; charset=utf-8")
        );
        assert_eq!(content_type("a.css"), Some("text/css; charset=utf-8"));
        assert_eq!(content_type("a.js.br"), None);
        assert_eq!(content_type("a.html"), None);
        assert_eq!(content_type("a.map"), None);
        assert_eq!(content_type("noext"), None);
    }

    #[test]
    fn build_rs_refuses_exactly_the_extensions_served_here() {
        let build = include_str!("../build.rs");
        let line = build
            .lines()
            .find(|l| l.starts_with("const ALLOWED"))
            .expect("build.rs ALLOWED");
        let quoted: Vec<&str> = line.split('"').skip(1).step_by(2).collect();
        let served: Vec<&str> = EXTENSIONS.iter().map(|(e, _)| *e).collect();
        assert_eq!(quoted, served);
    }

    #[test]
    fn index_html_and_compressed_names_are_never_assets() {
        assert!(asset("index.html").is_none());
        assert!(asset("x.js.br").is_none());
        assert!(asset("x.js.gz").is_none());
    }

    #[cfg(not(feature = "embed-ui"))]
    #[test]
    fn without_embed_ui_the_shell_is_the_placeholder_and_nothing_is_an_asset() {
        assert_eq!(status(), UiStatus::Placeholder);
        assert_eq!(shell_html(), PLACEHOLDER_HTML.as_bytes());
        assert!(asset("index-a1.js").is_none());
    }

    #[cfg(feature = "embed-ui")]
    #[test]
    fn with_embed_ui_the_shell_is_the_built_index_and_its_assets_are_served() {
        let UiStatus::Embedded { index_sha256 } = status() else {
            panic!("embed-ui reports a placeholder");
        };
        assert_eq!(index_sha256.len(), 12);
        let shell = std::str::from_utf8(shell_html()).expect("utf-8 index.html");
        assert!(shell.contains("<div id=\"app\">"), "{shell}");
        // Every script / stylesheet the shell names is an embedded asset.
        let names: Vec<&str> = shell
            .split("/-/assets/")
            .skip(1)
            .filter_map(|s| s.split('"').next())
            .collect();
        assert!(!names.is_empty(), "{shell}");
        for name in names {
            let a = asset(name).unwrap_or_else(|| panic!("{name} is not embedded"));
            assert!(!a.identity.is_empty(), "{name}");
        }
    }
}
