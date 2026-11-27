#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly

use super::*;

fn item(slug: &str, by: Option<&str>) -> SelectionItem {
    SelectionItem {
        slug: slug.to_owned(),
        version: "1.0.0".to_owned(),
        selected_by: by.map(str::to_owned),
        reason: None,
    }
}

#[test]
fn a_selection_is_the_persons_unless_the_recommendation_says_so() {
    let input = BuildRequest {
        selection: vec![
            item("claude-code-v2", Some("rule")),
            item("format-on-save", None),
        ],
        title: None,
    };
    let selected = selection_of(&input);
    assert_eq!(selected[0].selected_by, "rule");
    assert_eq!(selected[1].selected_by, "user");
    let recorded = selection_json(&selected);
    assert_eq!(recorded[1]["slug"], "format-on-save");
    assert_eq!(recorded[1]["selected_by"], "user");
}

#[test]
fn a_secret_in_a_built_file_is_refused() {
    let key = format!(
        "-----BEGIN {} PRIVATE KEY-----\nAAAA\n-----END {} PRIVATE KEY-----\n",
        "RSA", "RSA"
    );
    let writes = vec![
        ("CLAUDE.md".to_owned(), b"# fine\n".to_vec()),
        (".claude/rules/leak.md".to_owned(), key.into_bytes()),
    ];
    let err = scan_writes(&writes).expect_err("secret");
    assert_eq!(err.code(), ErrorCode::PushSecretDetected);
    assert!(scan_writes(&writes[..1]).is_ok());
}

#[test]
fn the_body_refuses_unknown_fields() {
    let err = serde_json::from_str::<BuildRequest>(r#"{"selection":[],"dry_run":true}"#);
    assert!(err.is_err());
    let ok: BuildRequest =
        serde_json::from_str(r#"{"selection":[{"slug":"a","version":"1.0.0"}]}"#).expect("body");
    assert_eq!(ok.selection.len(), 1);
}
