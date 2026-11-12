//! A job's artifacts on the runner (M4 C6, plan 14 §4.1; ADR-0029 §5):
//!
//! - **Before the script** ([`fetch_needed`]): the artifacts of the jobs
//!   this job `needs` (their current, succeeded attempt in the same
//!   pipeline — the server decides which), each written to its path under
//!   the work tree. A path is checked again here
//!   (`forge_core::ci_artifact::check_path`), every directory on the way is
//!   created by the runner (never followed through a link: the work tree is
//!   fresh, nothing in it is a link yet), and the file is created new.
//! - **After a successful script** ([`collect`], then the upload): every
//!   regular file under the declared `artifacts: { paths }`. Symbolic links
//!   are never followed or uploaded (a path through a link, or a link
//!   itself, is skipped and named in the log); a declared path that does
//!   not exist is named in the log. Over the attempt's bounds
//!   (`MAX_JOB_FILES`, `MAX_JOB_BYTES`, `MAX_FILE_BYTES`) nothing is sent
//!   (`FG-CI-0019`).
//!
//! Both use the job's own token (`JobArtifacts`), never the runner's.

use std::path::{Path, PathBuf};

use forge_core::ci_artifact::{self, MAX_FILE_BYTES, MAX_JOB_BYTES, MAX_JOB_FILES};
use forge_errors::{ErrorCode, ForgeError};

/// One file to upload: its path relative to the work tree and on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collected {
    pub path: String,
    pub file: PathBuf,
    pub size: u64,
}

/// What [`collect`] found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Collection {
    pub files: Vec<Collected>,
    /// Declared paths that do not exist.
    pub missing: Vec<String>,
    /// Paths skipped because they are, or pass through, a symbolic link
    /// (or are neither a file nor a directory).
    pub skipped: Vec<String>,
}

fn too_large(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::ArtifactTooLarge).with_context("reason", reason)
}

fn unreadable(e: std::io::Error) -> ForgeError {
    ForgeError::new(ErrorCode::ArtifactTransferFailed)
        .with_context("reason", "collect")
        .with_source(e)
}

/// Is any component of `rel` under `work` a symbolic link (or missing)?
fn through_link(work: &Path, rel: &str) -> std::io::Result<Option<bool>> {
    let mut at = work.to_path_buf();
    for part in rel.split('/') {
        at.push(part);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_symlink() => return Ok(Some(true)),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        }
    }
    Ok(Some(false))
}

/// The files under the declared `paths` of the work tree `work` (module
/// docs). Errors: `FG-CI-0019` past a bound, `FG-CI-0020 reason=collect`
/// when the tree cannot be read.
pub fn collect(work: &Path, paths: &[String]) -> Result<Collection, ForgeError> {
    let mut out = Collection::default();
    let mut total: u64 = 0;
    for declared in paths {
        if ci_artifact::check_path(declared).is_some() {
            out.skipped.push(declared.clone()); // clone: the log names it
            continue;
        }
        match through_link(work, declared).map_err(unreadable)? {
            None => {
                out.missing.push(declared.clone()); // clone: as above
                continue;
            }
            Some(true) => {
                out.skipped.push(declared.clone()); // clone: as above
                continue;
            }
            Some(false) => {}
        }
        let mut stack = vec![declared.clone()]; // clone: walked below
        while let Some(rel) = stack.pop() {
            let abs = work.join(&rel);
            let meta = std::fs::symlink_metadata(&abs).map_err(unreadable)?;
            if meta.is_dir() {
                let mut names: Vec<String> = Vec::new();
                for entry in std::fs::read_dir(&abs).map_err(unreadable)? {
                    let entry = entry.map_err(unreadable)?;
                    match entry.file_name().into_string() {
                        Ok(name) => names.push(name),
                        Err(_) => out.skipped.push(format!("{rel}/<not utf-8>")),
                    }
                }
                names.sort();
                for name in names.into_iter().rev() {
                    stack.push(format!("{rel}/{name}"));
                }
            } else if meta.is_file() {
                if ci_artifact::check_path(&rel).is_some() {
                    out.skipped.push(rel);
                    continue;
                }
                if meta.len() > MAX_FILE_BYTES {
                    return Err(too_large("file_limit")
                        .with_context("path", rel)
                        .with_context("limit_bytes", MAX_FILE_BYTES));
                }
                total = total.saturating_add(meta.len());
                if total > MAX_JOB_BYTES {
                    return Err(too_large("job_limit").with_context("limit_bytes", MAX_JOB_BYTES));
                }
                if i64::try_from(out.files.len()).unwrap_or(i64::MAX) >= MAX_JOB_FILES {
                    return Err(too_large("job_limit").with_context("limit_files", MAX_JOB_FILES));
                }
                if !out.files.iter().any(|f| f.path == rel) {
                    out.files.push(Collected {
                        size: meta.len(),
                        file: abs,
                        path: rel,
                    });
                }
            } else {
                // A link (never followed), a socket, a device.
                out.skipped.push(rel);
            }
        }
    }
    Ok(out)
}

/// Where a needed artifact `path` goes under `work`, its directories made
/// (refused: a bad path, or a file there already).
pub fn place(work: &Path, path: &str) -> Result<PathBuf, ForgeError> {
    let refused = |reason: &'static str| {
        ForgeError::new(ErrorCode::ArtifactTransferFailed)
            .with_context("reason", reason)
            .with_context("path", path.chars().take(300).collect::<String>())
    };
    if ci_artifact::check_path(path).is_some() {
        return Err(refused("path_invalid"));
    }
    if through_link(work, path)
        .map_err(|e| refused("place").with_source(e))?
        .is_some()
    {
        return Err(refused("path_exists"));
    }
    let target = work.join(path);
    if let Some(parent) = target.parent() {
        // Each component is checked again: none may be a link.
        let mut at = work.to_path_buf();
        for part in path
            .split('/')
            .collect::<Vec<_>>()
            .iter()
            .rev()
            .skip(1)
            .rev()
        {
            at.push(part);
            match std::fs::symlink_metadata(&at) {
                Ok(m) if m.is_dir() => {}
                Ok(_) => return Err(refused("path_exists")),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir(&at).map_err(|e| refused("place").with_source(e))?;
                }
                Err(e) => return Err(refused("place").with_source(e)),
            }
        }
        debug_assert!(parent.starts_with(work));
    }
    Ok(target)
}

/// Open `file` for an upload (M4 C6 review) without following a link:
/// it must still be a regular file, not a link, of the size `collect` saw,
/// and (Unix) the opened handle must be that very file (device and inode
/// compared, so a swap between the check and the open is caught). On
/// Windows the check is the link-free metadata before and after the open;
/// the work directory is the runner's own (no job runs any more).
pub fn open_regular(file: &Path, size: u64) -> Result<std::fs::File, ForgeError> {
    let refused = |reason: &'static str| {
        ForgeError::new(ErrorCode::ArtifactTransferFailed).with_context("reason", reason)
    };
    let before = std::fs::symlink_metadata(file).map_err(unreadable)?;
    if !before.file_type().is_file() || before.len() != size {
        return Err(refused("file_changed"));
    }
    let opened = std::fs::File::open(file).map_err(unreadable)?;
    let after = opened.metadata().map_err(unreadable)?;
    if !after.is_file() || after.len() != size {
        return Err(refused("file_changed"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return Err(refused("file_changed"));
        }
    }
    Ok(opened)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // tests may fail loudly

    use super::*;

    #[test]
    fn collect_takes_regular_files_and_names_what_it_skips() {
        let dir = tempfile::tempdir().expect("dir");
        let work = dir.path();
        std::fs::create_dir_all(work.join("out/sub")).expect("dirs");
        std::fs::write(work.join("out/a.txt"), b"a").expect("a");
        std::fs::write(work.join("out/sub/b.txt"), b"bb").expect("b");
        std::fs::write(work.join("report.html"), b"<p>").expect("r");
        let got = collect(
            work,
            &[
                "out".to_owned(),
                "report.html".to_owned(),
                "absent".to_owned(),
                "../escape".to_owned(),
            ],
        )
        .expect("collect");
        let paths: Vec<&str> = got.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["out/a.txt", "out/sub/b.txt", "report.html"]);
        assert_eq!(got.missing, ["absent"]);
        assert_eq!(got.skipped, ["../escape"]);
    }

    #[cfg(unix)]
    #[test]
    fn collect_never_follows_a_link() {
        let dir = tempfile::tempdir().expect("dir");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret"), b"x").expect("secret");
        let work = dir.path();
        std::fs::create_dir(work.join("out")).expect("out");
        std::os::unix::fs::symlink(outside.path(), work.join("out/link")).expect("link");
        std::os::unix::fs::symlink(outside.path(), work.join("linked")).expect("link");
        let got = collect(work, &["out".to_owned(), "linked".to_owned()]).expect("collect");
        assert!(got.files.is_empty(), "{got:?}");
        assert_eq!(got.skipped, ["out/link", "linked"]);
    }

    #[test]
    fn an_upload_opens_only_the_regular_file_collect_saw() {
        let dir = tempfile::tempdir().expect("dir");
        let f = dir.path().join("a.txt");
        std::fs::write(&f, b"abc").expect("write");
        assert!(open_regular(&f, 3).is_ok());
        // Changed since it was collected.
        assert!(open_regular(&f, 4).is_err());
        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&f, &link).expect("link");
            assert!(open_regular(&link, 3).is_err(), "a link is never opened");
        }
    }

    #[test]
    fn a_needed_artifact_is_placed_under_the_work_tree_only() {
        let dir = tempfile::tempdir().expect("dir");
        let work = dir.path();
        let at = place(work, "build/bin/app").expect("place");
        assert_eq!(at, work.join("build/bin/app"));
        assert!(work.join("build/bin").is_dir());
        std::fs::write(&at, b"x").expect("write");
        for bad in ["../x", "/etc/passwd", "build/bin/app", ".git/hooks/x"] {
            assert!(place(work, bad).is_err(), "{bad}");
        }
    }
}
