//! Writes the documents generated from the route registry and the permission
//! matrix: `docs/authz/routes.generated.json`, `docs/authz/matrix.generated.json`
//! and `docs/api/openapi.json` (paths relative to `Git/`).
//!
//! `cargo run -p forge-web --bin gen-authz`            — write
//! `cargo run -p forge-web --bin gen-authz -- --check` — fail if stale

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let check = std::env::args().any(|a| a == "--check");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut stale = Vec::new();
    for (rel, body) in forge_web::generated_documents(env!("CARGO_PKG_VERSION")) {
        let path = root.join(rel);
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        if current == body {
            continue;
        }
        if check {
            stale.push(rel);
            continue;
        }
        if let Some(dir) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("gen-authz: cannot create {}: {e}", dir.display());
                return ExitCode::FAILURE;
            }
        }
        if let Err(e) = std::fs::write(&path, body) {
            eprintln!("gen-authz: cannot write {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
        println!("wrote {rel}");
    }
    if stale.is_empty() {
        println!("authz manifests and OpenAPI are up to date");
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "stale: {} — run `cargo run -p forge-web --bin gen-authz` in Git/",
            stale.join(", ")
        );
        ExitCode::FAILURE
    }
}
