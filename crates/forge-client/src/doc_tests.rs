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

#[test]
fn the_windows_verbatim_prefix_is_stripped() {
    use super::strip_verbatim_str;
    assert_eq!(
        strip_verbatim_str(r"\\?\C:\work\repo").as_deref(),
        Some(r"C:\work\repo")
    );
    assert_eq!(
        strip_verbatim_str(r"\\?\UNC\srv\share\repo").as_deref(),
        Some(r"\\srv\share\repo")
    );
    // Not verbatim, or a verbatim form with no plain spelling: kept.
    assert_eq!(strip_verbatim_str(r"C:\work"), None);
    assert_eq!(strip_verbatim_str("/home/u/repo"), None);
    assert_eq!(strip_verbatim_str(r"\\?\Volume{x}\a"), None);
}

/// A git working tree with `files` (untracked: `git ls-files --others`).
fn work_tree(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let st = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(st.success());
    for (p, text) in files {
        let path = dir.path().join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    dir
}

fn manual(id: &str) -> String {
    format!("---\ndoctype: qms.manual\nid: {id}\ntitle: t\nowner: qa\n---\n\n本文。\n")
}

/// Review MR3: a real bound hit on the client — read_files names it, and
/// `gstn doc lint` exits 1 with FG-DOC-0002 although nothing else is wrong.
#[test]
fn a_bound_hit_on_the_client_exits_1_with_doc_index_bounded() {
    use super::{lint, read_files, report_lint, Limits};
    use clap::Parser as _;
    let t = work_tree(&[
        (".forge/doctypes/qms.manual.yml", "name: qms.manual\n"),
        ("QM-001.md", &manual("QM-001")),
        ("QM-002.md", &manual("QM-002")),
        ("QM-003.md", &manual("QM-003")),
    ]);
    let root = find_root(t.path()).unwrap();
    let all = read_files(&root, |_| true, &Limits::default()).unwrap();
    assert_eq!((all.markdown.len(), all.incomplete), (3, None));
    let few = Limits {
        markdown_files: 2,
        ..Limits::default()
    };
    let cut = read_files(&root, |_| true, &few).unwrap();
    assert_eq!((cut.markdown.len(), cut.incomplete), (2, Some("doc_files")));
    let bytes = Limits {
        total_bytes: 100,
        ..Limits::default()
    };
    assert_eq!(
        read_files(&root, |_| true, &bytes).unwrap().incomplete,
        Some("doc_bytes")
    );
    let types = Limits {
        doctype_files: 0,
        ..Limits::default()
    };
    assert_eq!(
        read_files(&root, |_| true, &types).unwrap().incomplete,
        Some("doctype_files")
    );
    let listing = Limits {
        listing_bytes: 20,
        ..Limits::default()
    };
    assert_eq!(
        read_files(&root, |_| true, &listing).unwrap().incomplete,
        Some("doc_tree")
    );
    let out = lint(&root, &[String::new()], false, &few).unwrap();
    assert_eq!(out.errors, 0);
    let cli = crate::cli::Cli::try_parse_from(["gstn", "doc", "lint"]).unwrap();
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let mut stdin = std::io::empty();
    let mut io = crate::app::Io {
        stdin: &mut stdin,
        stdout: &mut stdout,
        stderr: &mut stderr,
        stdin_tty: false,
        stdout_tty: false,
    };
    let e = report_lint(&cli, &mut io, &out).unwrap_err();
    assert_eq!(e.code_id(), "FG-DOC-0002");
    assert_eq!(e.exit().code(), 1);
    assert!(String::from_utf8_lossy(&stdout).contains("(doc_files)"));
}

/// Review MR3: fmt never writes over a file that changed since it was read.
#[test]
fn replace_file_refuses_a_file_that_changed_since_it_was_read() {
    use super::{replace_file, Stamp};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("QM-001.md");
    std::fs::write(&path, "old\n").unwrap();
    let stale = Stamp {
        len: 99,
        modified: None,
    };
    let e = replace_file(&path, b"new\n", Some(&stale)).unwrap_err();
    assert_eq!(e.code_id(), "FG-CLI-0007");
    assert!(
        e.detail()
            .unwrap_or_default()
            .contains("file_changed_while_formatting"),
        "{e}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\n");
    let leftovers = std::fs::read_dir(dir.path()).unwrap().count();
    assert_eq!(leftovers, 1, "the temporary file is removed");
    let fresh = Stamp::of(&std::fs::metadata(&path).unwrap());
    replace_file(&path, b"new\n", Some(&fresh)).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
}
