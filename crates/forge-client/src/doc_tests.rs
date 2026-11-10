#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly
//! `gstn doc`'s pieces (the commands end to end: tests/doc.rs).

use super::{find_root, is_doctype_file, selected, targets};

#[test]
fn targets_select_files_and_directories_by_whole_segments() {
    let all = vec![String::new()];
    assert!(selected(&all, "a/b.md"));
    let t = vec!["qms".to_owned(), "x/QM-001.md".to_owned()];
    assert!(selected(&t, "qms/QM-001.md"));
    assert!(selected(&t, "x/QM-001.md"));
    assert!(!selected(&t, "qms-old/QM-001.md"));
    assert!(!selected(&t, "x/QM-001.md.bak"));
}

#[test]
fn doctype_files_are_directly_in_the_directory() {
    assert!(is_doctype_file(".forge/doctypes/qms.manual.yml"));
    assert!(!is_doctype_file(".forge/doctypes/a/b.yml"));
    assert!(!is_doctype_file(".forge/doctypes/a.yaml"));
    assert!(!is_doctype_file("x/.forge/doctypes/a.yml"));
}

#[test]
fn the_root_is_found_upward_and_paths_are_made_relative_to_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
    std::fs::write(dir.path().join("a/b/QM-001.md"), "x").unwrap();
    let root = find_root(&dir.path().join("a/b")).expect("root");
    assert_eq!(root, dir.path().canonicalize().unwrap());
    let cwd = dir.path().join("a");
    let t = targets(
        &root,
        &cwd,
        &["b/QM-001.md".into(), ".".into(), "..".into()],
    )
    .expect("targets");
    assert_eq!(t, vec!["a/b/QM-001.md", "a", ""]);
    assert_eq!(targets(&root, &cwd, &[]).expect("all"), vec![""]);
    let e = targets(&root, &cwd, &["missing".into()]).unwrap_err();
    assert_eq!(e.code_id(), "FG-CLI-0001");
}
