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
//!   `[gpu_eval]` fails the job (`FG-CI-0029`) before any container.
//! - The GPU is `--device=<CDI name>` on the asking service's `podman
//!   create`; the model store is `--volume=<path>:<target>:ro,nosuid,nodev,
//!   noexec` on the same service. The pod stays `--network=none`.
//! - The model store is verified before every job that gets it (and at the
//!   runner's start): the manifest's sha256 is the configured one, every
//!   file it lists has its sha256, nothing else is in the store (no link, no
//!   unlisted file, no hard link), and (Unix) nothing in it is writable by
//!   group or others or owned by the runner's own user or its subordinate
//!   ids (the job containers' users). The store's path is canonical (no
//!   symbolic link on the way) and every directory above it is root's and
//!   not writable by group or others. Each file is opened without following
//!   a link and must be the file the walk saw (device, inode, one link).
//!   Anything else is `FG-CI-0030` (alert): the job fails and no container
//!   starts. The verification has its own deadline ([`VERIFY_TIMEOUT`]),
//!   outside the job's `timeout`.
//! - **Trust**: the store's owner (root, or the administrator who keeps the
//!   models) is trusted: a writer with the owner's rights could change a
//!   file between the verification and the service's read. The checks make
//!   sure nobody else — the runner, a job — can.

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
/// How long one verification before a job may take (its own deadline, not
/// the job's: FG-CI-0030 `reason=deadline` past it).
pub const VERIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1800);

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
    /// Refuse a file or directory the runner's own user or its subordinate
    /// ids own (they could make it writable). Off only in unit tests (a
    /// temporary directory is the test user's own).
    pub(crate) refuse_own_files: bool,
    /// Check that the path is canonical and every directory above it is
    /// root's and not writable by others. Off only in unit tests (a
    /// temporary directory is under the world-writable `/tmp`).
    pub(crate) check_ancestors: bool,
}

/// What the walk saw of a file: the open must find the same one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Seen {
    dev: u64,
    ino: u64,
}

impl Seen {
    #[cfg(unix)]
    fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt as _;
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
        }
    }
    #[cfg(not(unix))]
    fn of(_: &std::fs::Metadata) -> Self {
        Self { dev: 0, ino: 0 }
    }
}

/// The links of a file (1 off Unix).
fn links(meta: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        meta.nlink()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        1
    }
}

/// The subordinate id ranges (`start`, `count`) of the user `uid` in
/// `/etc/subuid` (`name:start:count` or `uid:start:count`), its name from
/// `/etc/passwd`. Unparsable lines are skipped: an id outside every range
/// is only refused when it is the runner's own.
#[cfg_attr(not(unix), allow(dead_code))]
fn subuid_ranges(passwd: &str, subuid: &str, uid: u32) -> Vec<(u32, u32)> {
    let uid_text = uid.to_string();
    let name = passwd.lines().find_map(|l| {
        let mut f = l.split(':');
        let name = f.next()?;
        let _password = f.next()?;
        (f.next()? == uid_text).then_some(name)
    });
    subuid
        .lines()
        .filter_map(|l| {
            let mut f = l.trim().split(':');
            let who = f.next()?;
            let start = f.next()?.parse::<u32>().ok()?;
            let count = f.next()?.parse::<u32>().ok()?;
            (who == uid_text || Some(who) == name).then_some((start, count))
        })
        .collect()
}

/// Whether `id` lies in one of `ranges`.
#[cfg_attr(not(unix), allow(dead_code))]
fn in_ranges(id: u32, ranges: &[(u32, u32)]) -> bool {
    ranges
        .iter()
        .any(|(start, count)| id >= *start && u64::from(id) < u64::from(*start) + u64::from(*count))
}

/// A directory above the store that someone other than root could change:
/// a link, not root's, or writable by group or others.
#[cfg_attr(not(unix), allow(dead_code))]
fn ancestor_unsafe(is_link: bool, uid: u32, mode: u32) -> bool {
    is_link || uid != 0 || mode & 0o022 != 0
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

/// Open `path` without following a link (`O_NOFOLLOW`, Unix) and check,
/// on the open handle, that it is the regular file the walk saw (device,
/// inode) with one link: a file swapped meanwhile is refused (`replaced`).
fn open_seen(path: &std::path::Path, seen: Seen) -> Result<std::fs::File, ForgeError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        #[allow(clippy::cast_possible_wrap)] // a flag bit, well inside i32
        options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    let file = options
        .open(path)
        .map_err(|e| unverified("unreadable").with_source(e))?;
    let meta = file
        .metadata()
        .map_err(|e| unverified("unreadable").with_source(e))?;
    if !meta.is_file() || Seen::of(&meta) != seen {
        return Err(unverified("replaced"));
    }
    if links(&meta) > 1 {
        return Err(unverified("hardlink"));
    }
    Ok(file)
}

impl ModelStore {
    pub fn new(path: PathBuf, target: String, manifest_sha256: String) -> Self {
        Self {
            path,
            target,
            manifest_sha256,
            refuse_own_files: true,
            check_ancestors: true,
        }
    }

    /// The store with its path resolved (`realpath`, Unix) — what is checked
    /// and mounted from then on (the configuration's load).
    pub fn canonical(&self) -> Result<Self, ForgeError> {
        let mut out = self.clone(); // clone: the resolved copy
        if cfg!(unix) {
            out.path = std::fs::canonicalize(&self.path)
                .map_err(|e| unverified("unreadable").with_source(e))?;
        }
        Ok(out)
    }

    /// The path is canonical and no directory above it can be changed by
    /// anyone but root (Unix; module docs).
    pub fn check_ancestors(&self) -> Result<(), ForgeError> {
        #[cfg(unix)]
        if self.check_ancestors {
            use std::os::unix::fs::MetadataExt as _;
            let real = std::fs::canonicalize(&self.path)
                .map_err(|e| unverified("unreadable").with_source(e))?;
            if real != self.path {
                return Err(unverified("not_canonical"));
            }
            for dir in self.path.ancestors().skip(1) {
                let meta = std::fs::symlink_metadata(dir).map_err(|e| {
                    unverified("unreadable")
                        .with_context("file", dir.display().to_string())
                        .with_source(e)
                })?;
                if ancestor_unsafe(meta.file_type().is_symlink(), meta.uid(), meta.mode()) {
                    return Err(unverified("ancestor_unsafe")
                        .with_context("file", dir.display().to_string()));
                }
            }
        }
        Ok(())
    }

    /// The ids whose files are refused: the runner's own and its
    /// subordinate ids (Unix; empty when not refusing).
    fn untrusted(&self) -> (Option<u32>, Vec<(u32, u32)>) {
        #[cfg(unix)]
        if self.refuse_own_files {
            let uid = rustix::process::geteuid().as_raw();
            let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
            return (
                Some(uid),
                subuid_ranges(&read("/etc/passwd"), &read("/etc/subuid"), uid),
            );
        }
        (None, Vec::new())
    }

    /// Why a file or directory could be changed by someone else (Unix;
    /// elsewhere the host's ACLs are the runbook's): writable by group or
    /// others, or owned by the runner or one of its subordinate ids.
    fn changeable(
        &self,
        meta: &std::fs::Metadata,
        untrusted: &(Option<u32>, Vec<(u32, u32)>),
    ) -> Option<&'static str> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if meta.mode() & 0o022 != 0 {
                return Some("writable");
            }
            if untrusted.0 == Some(meta.uid()) || in_ranges(meta.uid(), &untrusted.1) {
                return Some("owner_untrusted");
            }
            None
        }
        #[cfg(not(unix))]
        {
            let _ = (meta, untrusted);
            None
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
        self.check_ancestors()?;
        let untrusted = self.untrusted();
        let root = std::fs::symlink_metadata(&self.path)
            .map_err(|e| unverified("unreadable").with_source(e))?;
        if !root.is_dir() {
            return Err(unverified("not_a_directory"));
        }
        if let Some(why) = self.changeable(&root, &untrusted) {
            return Err(unverified(why).with_context("file", "."));
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
        let found = self.walk(&untrusted)?;
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
            let Some(seen) = found.get(path) else {
                return Err(unverified("missing_file").with_context("file", path.clone()));
                // clone: the error names it
            };
            let mut file = open_seen(&self.path.join(path), *seen)
                .map_err(|e| e.with_context("file", path.clone()))?; // clone: the error names it
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
        open_seen(&path, Seen::of(&meta))?
            .take(MAX_MANIFEST_BYTES)
            .read_to_end(&mut out)
            .map_err(|e| {
                unverified("unreadable")
                    .with_context("file", MANIFEST)
                    .with_source(e)
            })?;
        Ok(out)
    }

    /// Every regular file under the store (`/`-separated relative path →
    /// what was seen); a link (symbolic or hard), a device, a socket, a name
    /// that is not plain, a store too deep or too large, or anything others
    /// could change is refused.
    fn walk(
        &self,
        untrusted: &(Option<u32>, Vec<(u32, u32)>),
    ) -> Result<BTreeMap<String, Seen>, ForgeError> {
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
                if let Some(why) = self.changeable(&meta, untrusted) {
                    return Err(unverified(why).with_context("file", child));
                }
                if kind.is_dir() {
                    if depth + 1 >= MAX_DEPTH {
                        return Err(unverified("too_deep").with_context("file", child));
                    }
                    dirs.push((path, child, depth + 1));
                } else if kind.is_file() {
                    if links(&meta) > 1 {
                        return Err(unverified("hardlink").with_context("file", child));
                    }
                    files.insert(child, Seen::of(&meta));
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
            check_ancestors: false,
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

    /// FG-CI-0030: a changed file, a changed manifest, an extra file, a
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
            "owner_untrusted"
        );
    }

    /// Review: a hard link would let a writer elsewhere change a listed
    /// file; a file swapped after the walk is not the one opened.
    #[cfg(unix)]
    #[test]
    fn a_hard_link_or_a_replaced_file_is_refused() {
        let no = AtomicBool::new(false);
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        std::fs::hard_link(dir.path().join("config.json"), dir.path().join("x.json")).unwrap();
        assert_eq!(
            reason(&s.verify(later(), &no).expect_err("hardlink")),
            "hardlink"
        );
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        let other = Seen { dev: 0, ino: 0 };
        assert_eq!(
            reason(&open_seen(&file, other).expect_err("replaced")),
            "replaced"
        );
        // A link at the path is never followed.
        std::os::unix::fs::symlink(&file, dir.path().join("l")).unwrap();
        let seen = Seen::of(&std::fs::metadata(&file).unwrap());
        assert_eq!(
            reason(&open_seen(&dir.path().join("l"), seen).expect_err("link")),
            "unreadable"
        );
        open_seen(&file, seen).expect("the file itself");
    }

    /// Review: the store's path must be canonical (no link on the way) and
    /// the directories above it root's and closed.
    #[cfg(unix)]
    #[test]
    fn a_store_behind_a_link_or_under_an_open_directory_is_refused() {
        let no = AtomicBool::new(false);
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let s = store(&real);
        std::os::unix::fs::symlink(&real, dir.path().join("via")).unwrap();
        let linked = ModelStore {
            path: dir.path().join("via"),
            check_ancestors: true,
            ..s.clone()
        };
        assert_eq!(
            reason(&linked.verify(later(), &no).expect_err("link")),
            "not_canonical"
        );
        assert_eq!(
            linked.canonical().expect("resolved").path,
            std::fs::canonicalize(&real).unwrap()
        );
        // The temporary directory is the test user's, under /tmp: unsafe.
        let checked = ModelStore {
            path: std::fs::canonicalize(&real).unwrap(),
            check_ancestors: true,
            ..s
        };
        assert_eq!(
            reason(&checked.verify(later(), &no).expect_err("ancestor")),
            "ancestor_unsafe"
        );
    }

    #[test]
    fn subordinate_ids_and_unsafe_ancestors_are_recognised() {
        let passwd =
            "root:x:0:0::/root:/bin/sh\nforge-runner:x:990:990::/var/lib/forge-runner:/sbin/nologin\n";
        let subuid = "forge-runner:100000:65536\n991:200000:10\nother:300000:5\nbroken\n";
        let ranges = subuid_ranges(passwd, subuid, 990);
        assert_eq!(ranges, [(100_000, 65_536)]);
        assert!(in_ranges(100_000, &ranges) && in_ranges(165_535, &ranges));
        assert!(!in_ranges(165_536, &ranges) && !in_ranges(99_999, &ranges));
        assert_eq!(subuid_ranges(passwd, subuid, 991), [(200_000, 10)]);
        assert!(!ancestor_unsafe(false, 0, 0o755));
        assert!(ancestor_unsafe(true, 0, 0o755));
        assert!(ancestor_unsafe(false, 1000, 0o755));
        assert!(ancestor_unsafe(false, 0, 0o1777));
        assert!(ancestor_unsafe(false, 0, 0o775));
    }
}
