//! The job's directories (M4 C4): nothing survives from one job to the
//! next. Every job gets `<work_dir>/job/{work,script}` freshly created
//! after whatever was there is removed, and the whole `job` directory is
//! removed when the job ends. A directory the runner cannot remove (files
//! the container created under its subordinate ids) is removed through
//! `podman unshare`; if it still exists the runner takes no further job
//! (`WorkspaceDirty`).

use std::path::{Path, PathBuf};

/// One job's directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobDirs {
    /// `<work_dir>/job`: removed whole.
    pub base: PathBuf,
    /// The checkout, mounted read-write at `/workspace`.
    pub work: PathBuf,
    /// `script.sh`, mounted read-only at `/forge`.
    pub script: PathBuf,
    /// The job's secrets as Podman's env file (M4 C6): owner only, never
    /// mounted; read by `podman create` and removed right after it.
    pub secrets: PathBuf,
}

/// The runner's work directory.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn dirs(&self) -> JobDirs {
        let base = self.root.join("job");
        JobDirs {
            work: base.join("work"),
            script: base.join("script"),
            secrets: base.join("secrets"),
            base,
        }
    }

    /// Remove the job directory if it exists (std first).
    pub fn wipe(&self) -> std::io::Result<()> {
        let base = self.dirs().base;
        match std::fs::remove_dir_all(&base) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Whether anything of a job is left.
    pub fn is_clean(&self) -> bool {
        !self.dirs().base.exists()
    }

    /// Fresh, empty directories for the next job (the caller wiped first).
    pub fn create(&self) -> std::io::Result<JobDirs> {
        let dirs = self.dirs();
        std::fs::create_dir_all(&dirs.work)?;
        std::fs::create_dir_all(&dirs.script)?;
        Ok(dirs)
    }

    /// Write the job's secrets as Podman's env file (`NAME=value` per line;
    /// the caller checked every name and value: one line each) in a
    /// directory only the runner's user may read (Unix: 0700 / 0600).
    pub fn write_env_file(dirs: &JobDirs, vars: &[(&str, &str)]) -> std::io::Result<PathBuf> {
        use std::io::Write as _;
        std::fs::create_dir_all(&dirs.secrets)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&dirs.secrets, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = dirs.secrets.join("job.env");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        let mut text = zeroize::Zeroizing::new(String::new());
        for (name, value) in vars {
            text.push_str(name);
            text.push('=');
            text.push_str(value);
            text.push('\n');
        }
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        Ok(path)
    }

    /// Write the job's script (the definition's lines, one per line).
    pub fn write_script(dirs: &JobDirs, lines: &[String]) -> std::io::Result<PathBuf> {
        let path = dirs.script.join("script.sh");
        let mut text = String::new();
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
        std::fs::write(&path, text)?;
        Ok(path)
    }

    /// The bytes of every file under the job's work directory (symbolic
    /// links not followed; a link counts its own size).
    pub fn work_size(&self) -> std::io::Result<u64> {
        fn walk(dir: &Path) -> std::io::Result<u64> {
            let mut total = 0u64;
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                let meta = std::fs::symlink_metadata(entry.path())?;
                total = total.saturating_add(if meta.is_dir() {
                    walk(&entry.path())?
                } else {
                    meta.len()
                });
            }
            Ok(total)
        }
        walk(&self.dirs().work)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // tests may fail loudly

    use super::*;

    #[test]
    fn a_wipe_leaves_nothing_and_create_starts_empty() {
        let dir = tempfile::tempdir().expect("dir");
        let ws = Workspace::new(dir.path().to_path_buf());
        let first = ws.create().expect("create");
        std::fs::write(first.work.join("leftover"), b"x").expect("write");
        ws.wipe().expect("wipe");
        assert!(ws.is_clean());
        let second = ws.create().expect("create");
        assert_eq!(std::fs::read_dir(&second.work).expect("read").count(), 0);
        ws.wipe().expect("wipe");
        ws.wipe().expect("a second wipe is fine");
    }

    #[test]
    fn the_env_file_holds_one_line_per_secret_outside_the_mounts() {
        let dir = tempfile::tempdir().expect("dir");
        let ws = Workspace::new(dir.path().to_path_buf());
        let dirs = ws.create().expect("create");
        let path = Workspace::write_env_file(&dirs, &[("A_TOKEN", "v1-long"), ("B", "x=y")])
            .expect("env file");
        assert!(!path.starts_with(&dirs.work) && !path.starts_with(&dirs.script));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "A_TOKEN=v1-long\nB=x=y\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        ws.wipe().expect("wipe");
        assert!(ws.is_clean());
    }
}
