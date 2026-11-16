//! A project's `ai_assets.enabled` (plan 17 §0.4, P17-3, ADR-0035): the
//! opt-in of the AI asset lint, the `forge/risk` rules `ai_case_raw_data` /
//! `ai_asset` and (P17-7) the asset releases. Off by default; only a person
//! who is Maintainer or above, in a web session, turns it on or off
//! (`AiAssetsSettingManage`), audited. Turning it off stops those checks
//! and new releases only: history, past releases, signatures and
//! deployment records stay.

/// A person turned `ai_assets.enabled` on or off (audited on the project,
/// no event).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AiAssetsChange {
    pub project: crate::ids::ProjectId,
    pub from: bool,
    pub to: bool,
}

impl AiAssetsChange {
    /// `ai_assets.enabled` / `ai_assets.disabled`.
    pub fn action(&self) -> &'static str {
        if self.to {
            "ai_assets.enabled"
        } else {
            "ai_assets.disabled"
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        serde_json::json!({
            "project_id": self.project.get(),
            "from": self.from,
            "to": self.to,
        })
    }
}
