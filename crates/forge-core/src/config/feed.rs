//! `[feed]`: the event feed of integrations (M3 I1c, ADR-0016 §7,
//! ADR-0018 §5; plan 11 Q1).
//!
//! `feed.require_sink_compare` (default `false`): close the integrations'
//! feed (`503`, `FG-DB-0012`) until this process compared the audit seal
//! with the seal sink (`forge seal-sink`, ADR-0018 §5). The seal sink does
//! not exist yet (C9), so in M3 the only accepted value is `false` — the
//! feed answers without the comparison and `forge doctor` warns
//! (`feed.sink_compare`). `true` is refused at load time (`FG-SYS-0002
//! reason=unsupported`): a setting that cannot take effect must not look
//! as if it did. Once the seal sink ships, the default becomes `true`.

use forge_errors::ForgeError;

use super::invalid;

/// The key (named by errors and doctor).
pub const REQUIRE_SINK_COMPARE_KEY: &str = "feed.require_sink_compare";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FeedConfig {
    /// `feed.require_sink_compare` (always `false` in M3).
    pub require_sink_compare: bool,
}

impl FeedConfig {
    pub(super) fn load<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Result<Self, ForgeError> {
        match get(REQUIRE_SINK_COMPARE_KEY) {
            None | Some("false") => Ok(Self {
                require_sink_compare: false,
            }),
            Some("true") => Err(invalid(REQUIRE_SINK_COMPARE_KEY, "unsupported")),
            Some(_) => Err(invalid(REQUIRE_SINK_COMPARE_KEY, "invalid")),
        }
    }
}
