//! The GPU runner (plan 17 P17-1, ADR-0029 amendment 2026-11-14): what the
//! runner host's configuration (`[gpu_eval]`) gives a **service** of a job
//! that asked for it — never the job's own container.
//!
//! ```toml
//! [gpu_eval]
//! devices = ["nvidia.com/gpu=all"]        # CDI names only, never a /dev path
//! [gpu_eval.model_store]
//! path = "/srv/forge-models"              # owned by another user, not writable
//! target = "/models"                      # where the service sees it
//! manifest_sha256 = "<64 hex>"            # sha256 of <path>/forge-models.sha256
//! ```
//!
//! - `.forge-ci.yml` only asks: a service's `gpu: true`, in a job tagged
//!   `gpu-eval` (forge-ci's `service_gpu_without_tag`). A runner tagged
//!   `gpu-eval` takes only jobs that ask for it (forge-db's claim), so an
//!   untagged job never runs here. A service that asks on a runner without
//!   `[gpu_eval]` fails the job (`FG-CI-0026`) before any container.
//! - The GPU is `--device=<CDI name>` on the asking service's `podman
//!   create`; the model store is `--volume=<path>:<target>:ro,nosuid,nodev,
//!   noexec` on the same service. The pod stays `--network=none`.
//! - The model store is verified before every job that gets it (and at the
//!   runner's start): the manifest's sha256 is the configured one, every
//!   file it lists has its sha256, nothing else is in the store (no link, no
//!   unlisted file), and (Unix) nothing in it is writable by group or others
//!   or owned by the runner's own user. Anything else is `FG-CI-0027`
//!   (alert): the job fails and no container starts.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use forge_errors::{ErrorCode, ForgeError};
use sha2::{Digest, Sha256};

/// The manifest at the store's root: `sha256sum` lines `<64 hex>  <path>`.
pub const MANIFEST: &str = "forge-models.sha256";
/// At most this many files listed (and entries found) in a store.
pub const MAX_FILES: usize = 4096;
/// At most this many CDI devices.
pub const MAX_DEVICES: usize = 8;
/// The deepest directory of a store.
const MAX_DEPTH: usize = 16;
/// The largest manifest read.
const MAX_MANIFEST_BYTES: u64 = 1 << 20;
/// Bytes hashed between two checks of the stop flag and the deadline.
const CHUNK: usize = 1 << 20;

/// The runner host's GPU configuration (`[gpu_eval]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuEval {
    /// CDI device names ([`valid_cdi_device`]).
    pub devices: Vec<String>,
    pub model_store: Option<ModelStore>,
}

/// The read-only model store (`[gpu_eval.model_store]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelStore {
    /// Absolute, on the runner host.
    pub path: PathBuf,
    /// Absolute, in the service container ([`valid_target`]).
    pub target: String,
    /// The manifest's sha256 (64 lower-case hex).
    pub manifest_sha256: String,
    /// Refuse a file or directory the runner's own user owns (it could make
    /// it writable). Off only in unit tests (a temporary directory is the
    /// test user's own).
    pub(crate) refuse_own_files: bool,
}

/// How a verification ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verified {
    /// Every listed file has its digest and nothing else is there.
    Ok { files: usize, bytes: u64 },
    /// The job was stopped meanwhile.
    Stopped,
    /// The deadline passed meanwhile.
    TimedOut,
}

fn unverified(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::ModelStoreUnverified).with_context("reason", reason)
}

/// `vendor.example/class=name` (the Container Device Interface's fully
/// qualified name): a vendor with a dot, never a device path.
pub fn valid_cdi_device(name: &str) -> bool {
    let Some((kind, device)) = name.split_once('=') else {
        return false;
    };
    let Some((vendor, class)) = kind.split_once('/') else {
        return false;
    };
    let starts_alnum = |s: &str| {
        s.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    };
    name.len() <= 128
        && starts_alnum(vendor)
        && vendor.contains('.')
        && vendor
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-".contains(&b))
        && starts_alnum(class)
        && class
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b))
        && device
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && device
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
}

/// The store's path in the container: absolute, plain components, not `/`
/// and not over `/proc`, `/sys`, `/dev`, `/tmp` or the job's own mounts.
pub fn valid_target(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    let parts: Vec<&str> = rest.split('/').collect();
    path.len() <= 255
        && !rest.is_empty()
        && parts.iter().all(|c| plain_component(c))
        && !["proc", "sys", "dev", "tmp", "workspace", "forge"].contains(&parts[0])
}

/// `[A-Za-z0-9._+-]`, not empty, not `.` or `..`.
fn plain_component(c: &str) -> bool {
    !c.is_empty()
        && c != "."
        && c != ".."
        && c.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}

/// A manifest path: relative, `/`-separated plain components.
fn manifest_path(p: &str) -> bool {
    p.len() <= 1024 && p.split('/').count() <= MAX_DEPTH && p.split('/').all(plain_component)
}

fn sha256_hex(digest: impl AsRef<[u8]>) -> String {
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `sha256sum` lines (`<hex>  <path>` or `<hex> *<path>`), empty lines
/// skipped; every path once.
fn parse_manifest(text: &str) -> Result<BTreeMap<String, String>, ForgeError> {
    let mut out = BTreeMap::new();
    for line in text.lines().filter(|l| !l.is_empty()) {
        let parsed = line.split_at_checked(64).and_then(|(hex, rest)| {
            let path = rest
                .strip_prefix("  ")
                .or_else(|| rest.strip_prefix(" *"))?;
            (is_sha256(hex) && manifest_path(path) && path != MANIFEST).then_some((hex, path))
        });
        let Some((hex, path)) = parsed else {
            return Err(unverified("manifest_invalid").with_context("line", out.len() + 1));
        };
        if out.insert(path.to_owned(), hex.to_owned()).is_some() {
            return Err(unverified("manifest_invalid").with_context("file", path.to_owned()));
        }
        if out.len() > MAX_FILES {
            return Err(unverified("too_many_files"));
        }
    }
    Ok(out)
}

impl ModelStore {
    pub fn new(path: PathBuf, target: String, manifest_sha256: String) -> Self {
        Self {
            path,
            target,
            manifest_sha256,
            refuse_own_files: true,
        }
    }

    /// A file or directory the service could be handed changed: writable by
    /// group or others, or the runner's own (Unix; elsewhere the host's ACLs
    /// are the runbook's).
    fn writable(&self, meta: &std::fs::Metadata) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            meta.mode() & 0o022 != 0
                || (self.refuse_own_files && meta.uid() == rustix::process::geteuid().as_raw())
        }
        #[cfg(not(unix))]
        {
            let _ = (meta, self.refuse_own_files);
            false
        }
    }

    /// Verify the store (module docs). Checks `stop` and `deadline` between
    /// files and every [`CHUNK`] bytes.
    pub fn verify(&self, deadline: Instant, stop: &AtomicBool) -> Result<Verified, ForgeError> {
        let interrupted = || {
            if stop.load(Ordering::SeqCst) {
                Some(Verified::Stopped)
            } else if Instant::now() >= deadline {
                Some(Verified::TimedOut)
            } else {
                None
            }
        };
        let root = std::fs::symlink_metadata(&self.path)
            .map_err(|e| unverified("unreadable").with_source(e))?;
        if !root.is_dir() {
            return Err(unverified("not_a_directory"));
        }
        if self.writable(&root) {
            return Err(unverified("writable").with_context("file", "."));
        }
        // 1. The manifest is the configured one.
        let manifest = self.read_manifest()?;
        let actual = sha256_hex(Sha256::digest(&manifest));
        if actual != self.manifest_sha256 {
            return Err(unverified("manifest_digest")
                .with_context("expected", self.manifest_sha256.clone()) // clone: the error names it
                .with_context("actual", actual));
        }
        let text = std::str::from_utf8(&manifest).map_err(|_| unverified("manifest_invalid"))?;
        let listed = parse_manifest(text)?;
        // 2. Nothing but the listed files (and the manifest) is there.
        let found = self.walk()?;
        if let Some(extra) = found
            .keys()
            .find(|p| p.as_str() != MANIFEST && !listed.contains_key(*p))
        {
            // clone: the error names it
            return Err(unverified("unlisted_file").with_context("file", extra.clone()));
        }
        // 3. Every listed file has its digest.
        let mut bytes = 0u64;
        let mut buf = vec![0u8; CHUNK];
        for (path, want) in &listed {
            if let Some(end) = interrupted() {
                return Ok(end);
            }
            if !found.contains_key(path) {
                return Err(unverified("missing_file").with_context("file", path.clone()));
                // clone: the error names it
            }
            let mut file = std::fs::File::open(self.path.join(path)).map_err(|e| {
                unverified("unreadable")
                    .with_context("file", path.clone())
                    .with_source(e)
            })?;
            let mut hasher = Sha256::new();
            loop {
                let n = file.read(&mut buf).map_err(|e| {
                    unverified("unreadable")
                        .with_context("file", path.clone()) // clone: the error names it
                        .with_source(e)
                })?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                bytes = bytes.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
                if let Some(end) = interrupted() {
                    return Ok(end);
                }
            }
            let got = sha256_hex(hasher.finalize());
            if got != *want {
                return Err(unverified("file_digest")
                    .with_context("file", path.clone()) // clone: the error names it
                    .with_context("expected", want.clone())
                    .with_context("actual", got));
            }
        }
        Ok(Verified::Ok {
            files: listed.len(),
            bytes,
        })
    }

    fn read_manifest(&self) -> Result<Vec<u8>, ForgeError> {
        let path = self.path.join(MANIFEST);
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| unverified("manifest_missing").with_source(e))?;
        if !meta.is_file() {
            return Err(unverified("not_regular").with_context("file", MANIFEST));
        }
        if meta.len() > MAX_MANIFEST_BYTES {
            return Err(unverified("manifest_invalid").with_context("reason_detail", "too_large"));
        }
        let mut out = Vec::new();
        std::fs::File::open(&path)
            .and_then(|f| f.take(MAX_MANIFEST_BYTES).read_to_end(&mut out))
            .map_err(|e| {
                unverified("unreadable")
                    .with_context("file", MANIFEST)
                    .with_source(e)
            })?;
        Ok(out)
    }

    /// Every regular file under the store (`/`-separated relative path →
    /// size); a link, a device, a socket, a name that is not plain, a store
    /// too deep or too large, or anything writable is refused.
    fn walk(&self) -> Result<BTreeMap<String, u64>, ForgeError> {
        // The size is what was seen; the hash reads the file itself.
        let mut files = BTreeMap::new();
        let mut entries = 0usize;
        let mut dirs: Vec<(PathBuf, String, usize)> = vec![(self.path.clone(), String::new(), 0)];
        while let Some((dir, rel, depth)) = dirs.pop() {
            let read = std::fs::read_dir(&dir).map_err(|e| {
                unverified("unreadable")
                    .with_context("file", rel.clone())
                    .with_source(e)
            })?;
            for entry in read {
                let entry = entry.map_err(|e| unverified("unreadable").with_source(e))?;
                entries += 1;
                if entries > 2 * MAX_FILES {
                    return Err(unverified("too_many_files"));
                }
                let name = entry.file_name();
                let Some(name) = name.to_str().filter(|n| plain_component(n)) else {
                    return Err(unverified("name_invalid").with_context("file", rel.clone()));
                    // clone: the error names its directory
                };
                let child = if rel.is_empty() {
                    name.to_owned()
                } else {
                    format!("{rel}/{name}")
                };
                let path = entry.path();
                let meta = std::fs::symlink_metadata(&path).map_err(|e| {
                    unverified("unreadable")
                        .with_context("file", child.clone())
                        .with_source(e)
                })?;
                let kind = meta.file_type();
                if kind.is_symlink() {
                    return Err(unverified("link").with_context("file", child));
                }
                if self.writable(&meta) {
                    return Err(unverified("writable").with_context("file", child));
                }
                if kind.is_dir() {
                    if depth + 1 >= MAX_DEPTH {
                        return Err(unverified("too_deep").with_context("file", child));
                    }
                    dirs.push((path, child, depth + 1));
                } else if kind.is_file() {
                    files.insert(child, meta.len());
                } else {
                    return Err(unverified("not_regular").with_context("file", child));
                }
            }
        }
        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

    use std::path::Path;
    use std::time::Duration;

    use super::*;

    #[test]
    fn only_cdi_names_are_devices() {
        for good in [
            "nvidia.com/gpu=all",
            "nvidia.com/gpu=0",
            "amd.com/gpu=GPU-1:a_b",
        ] {
            assert!(valid_cdi_device(good), "{good}");
        }
        for bad in [
            "/dev/nvidia0",
            "nvidia/gpu=all",
            "nvidia.com/gpu",
            "nvidia.com/gpu=",
            "nvidia.com/gpu=all,rw",
            "nvidia.com/gpu=all --privileged",
            "NVIDIA.com/gpu=all",
            "",
        ] {
            assert!(!valid_cdi_device(bad), "{bad}");
        }
    }

    #[test]
    fn the_target_is_a_plain_path_outside_the_jobs_mounts() {
        assert!(valid_target("/models"));
        assert!(valid_target("/root/.ollama/models"));
        for bad in [
            "models",
            "/",
            "/proc/x",
            "/dev",
            "/workspace",
            "/forge/m",
            "/tmp/m",
            "/a/../b",
            "/a:/b",
            "/a,ro",
            "/a//b",
        ] {
            assert!(!valid_target(bad), "{bad}");
        }
    }

    /// A store with two files and its manifest; the configured digest.
    fn store(dir: &Path) -> ModelStore {
        std::fs::create_dir_all(dir.join("blobs")).unwrap();
        std::fs::write(dir.join("blobs/sha256-1"), b"weights").unwrap();
        std::fs::write(dir.join("config.json"), b"{}").unwrap();
        let manifest = format!(
            "{}  blobs/sha256-1\n{}  config.json\n",
            sha256_hex(Sha256::digest(b"weights")),
            sha256_hex(Sha256::digest(b"{}"))
        );
        std::fs::write(dir.join(MANIFEST), &manifest).unwrap();
        ModelStore {
            refuse_own_files: false,
            ..ModelStore::new(
                dir.to_path_buf(),
                "/models".to_owned(),
                sha256_hex(Sha256::digest(manifest.as_bytes())),
            )
        }
    }

    fn reason(e: &ForgeError) -> String {
        e.context()
            .iter()
            .find(|(k, _)| *k == "reason")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    #[test]
    fn a_store_matching_its_manifest_is_verified() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        assert_eq!(
            s.verify(later(), &AtomicBool::new(false))
                .expect("verified"),
            Verified::Ok { files: 2, bytes: 9 }
        );
        // Stopped or out of time: no verdict either way.
        assert_eq!(
            s.verify(later(), &AtomicBool::new(true)).expect("stopped"),
            Verified::Stopped
        );
        assert_eq!(
            s.verify(Instant::now(), &AtomicBool::new(false))
                .expect("late"),
            Verified::TimedOut
        );
    }

    /// FG-CI-0027: a changed file, a changed manifest, an extra file, a
    /// missing file — each fails before any container gets the store.
    #[test]
    fn a_store_that_differs_from_its_digests_is_refused() {
        let no = AtomicBool::new(false);
        type Change = fn(&Path);
        let cases: [(&str, Change); 4] = [
            ("file_digest", |d| {
                std::fs::write(d.join("config.json"), b"{\"x\":1}").unwrap()
            }),
            ("manifest_digest", |d| {
                std::fs::write(d.join(MANIFEST), b"").unwrap();
            }),
            ("unlisted_file", |d| {
                std::fs::write(d.join("blobs/extra"), b"x").unwrap()
            }),
            ("missing_file", |d| {
                std::fs::remove_file(d.join("config.json")).unwrap()
            }),
        ];
        for (want, change) in cases {
            let dir = tempfile::tempdir().unwrap();
            let s = store(dir.path());
            change(dir.path());
            let e = s.verify(later(), &no).expect_err(want);
            assert_eq!(e.code(), ErrorCode::ModelStoreUnverified);
            assert_eq!(reason(&e), want);
        }
        let missing = ModelStore::new(
            PathBuf::from("/nonexistent/forge-models"),
            "/models".to_owned(),
            "a".repeat(64),
        );
        assert_eq!(
            reason(&missing.verify(later(), &no).expect_err("missing")),
            "unreadable"
        );
    }

    #[test]
    fn a_manifest_names_only_plain_relative_paths() {
        let h = "a".repeat(64);
        for bad in [
            format!("{h}  ../etc/passwd"),
            format!("{h}  /etc/passwd"),
            format!("{h}  a//b"),
            format!("{h} a"),
            format!("{h}  {MANIFEST}"),
            format!("{}  a", "A".repeat(64)),
            format!("{h}  a\n{h}  a"),
        ] {
            assert!(parse_manifest(&bad).is_err(), "{bad}");
        }
        assert_eq!(
            parse_manifest(&format!("{h}  a/b\n\n{h} *c\n"))
                .expect("valid")
                .len(),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_or_a_writable_file_in_the_store_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let no = AtomicBool::new(false);
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        std::os::unix::fs::symlink("/etc/passwd", dir.path().join("blobs/link")).unwrap();
        assert_eq!(reason(&s.verify(later(), &no).expect_err("link")), "link");
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        std::fs::set_permissions(
            dir.path().join("config.json"),
            std::fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        assert_eq!(
            reason(&s.verify(later(), &no).expect_err("writable")),
            "writable"
        );
        // The runner's own files: it could make them writable.
        let dir = tempfile::tempdir().unwrap();
        let s = ModelStore {
            refuse_own_files: true,
            ..store(dir.path())
        };
        assert_eq!(
            reason(&s.verify(later(), &no).expect_err("own")),
            "writable"
        );
    }
}
