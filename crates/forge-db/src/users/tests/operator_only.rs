//! The operator-only paths past the administration guard (ADR-0019 §10):
//! `Guard::Force`, `disable_with` and `bootstrap_admin` are for the host
//! operator's CLI. HTTP never gets them; this source scan keeps it so,
//! like forge-web's `no_router_bypass_outside_the_registry`.

use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn forge_web_never_names_the_operator_only_user_commands() {
    // Assembled at run time so this file does not match itself.
    let forbidden = [
        format!("{}{}", "Guard::", "Force"),
        format!("{}{}", "disable_", "with"),
        format!("{}{}", "bootstrap_", "admin"),
    ];
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../forge-web/src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(!files.is_empty(), "no sources under {}", src.display());
    let mut offenders = Vec::new();
    for file in files {
        let text =
            std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        for pat in &forbidden {
            if text.contains(pat.as_str()) {
                offenders.push(format!("{}: {pat}", file.display()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "HTTP must not reach the operator-only user commands: {offenders:?}"
    );
}
