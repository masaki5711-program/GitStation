//! A project's wiki link (plan 20 W1/W2, ADR-0040 §5): the wiki is an
//! ordinary project in the same namespace, linked to its owner project by
//! one row of `forge.project_wikis`; the owner's members inherit their role
//! on it (one way, one hop; `forge_db::roles`). Linking and unlinking are a
//! person's decision (`ProjectWikiLink`), audited on the owner project.

use crate::ids::ProjectId;

/// The default path segment of a project's wiki: `<owner segment>-wiki`.
/// `None` when the result would be longer than a segment may be (the
/// caller asks for a path then).
pub fn default_wiki_segment(owner_segment: &str) -> Option<String> {
    let segment = format!("{owner_segment}-wiki");
    crate::namespace_path::PathSegment::parse(&segment)
        .ok()
        .map(|_| segment)
}

/// A wiki linked to (`linked`) or unlinked from its owner project (audited
/// on the owner project, no event).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WikiLinkChange {
    /// The owner project.
    pub project: ProjectId,
    /// The wiki project.
    pub wiki: ProjectId,
    pub linked: bool,
}

impl WikiLinkChange {
    /// `project.wiki_linked` / `project.wiki_unlinked`.
    pub fn action(&self) -> &'static str {
        if self.linked {
            "project.wiki_linked"
        } else {
            "project.wiki_unlinked"
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        serde_json::json!({
            "project_id": self.project.get(),
            "wiki_project_id": self.wiki.get(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_segment_appends_wiki() {
        assert_eq!(default_wiki_segment("web").as_deref(), Some("web-wiki"));
    }

    #[test]
    fn link_actions_are_named() {
        let mut c = WikiLinkChange {
            project: ProjectId::new(1),
            wiki: ProjectId::new(2),
            linked: true,
        };
        assert_eq!(c.action(), "project.wiki_linked");
        c.linked = false;
        assert_eq!(c.action(), "project.wiki_unlinked");
        assert_eq!(c.to_value()["wiki_project_id"], 2);
    }
}
