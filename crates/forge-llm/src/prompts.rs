//! The versioned prompts (plan 19 §2.1: `crates/forge-llm/prompts/
//! <purpose>.v<N>.txt`, compiled in). A change of a prompt's text is a new
//! version (a new file), so every advice names the prompt it came from.

use crate::Prompt;

/// The approval summary (M5 L5-8, plan 19 §3.4): three lines in Japanese
/// business words, no verdict, the facts' own words only.
pub fn approval_summary() -> Prompt {
    Prompt {
        id: "approval_summary".to_owned(),
        version: 1,
        system: include_str!("../prompts/approval_summary.v1.txt").to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_approval_prompt_asks_for_no_verdict_and_the_checked_shape() {
        let p = approval_summary();
        assert_eq!(p.id, "approval_summary");
        assert_eq!(p.version, 1);
        assert!(p.system.contains(r#"{"summary": "...", "findings": []}"#));
        assert!(p.system.contains("never make it"));
    }
}
