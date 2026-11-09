//! The repository archive inside the encrypted stream (M3 O1) and the
//! file copy of one repository.
//!
//! ```text
//! entry = 1 | path            (a directory)
//!       | 2 | path | size u64 | bytes   (a regular file)
//!       | 0                    (the end; nothing may follow)
//! path  = length u16 | UTF-8, '/'-separated, relative to the storage root
//! ```
//!
//! A repository is copied **refs before objects** (loose refs, then
//! `packed-refs`, then the rest, then `objects/`): every ref of the copy
//! names an object that existed when the ref was read, and objects are
//! never rewritten in place. A gc that replaces packs during the copy can
//! still lose an object; `forge backup` therefore checks the copy with
//! `git fsck --connectivity-only` and copies again. Lock files, `gc.pid`
//! and push quarantines are not copied.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

const DIR: u8 = 1;
const FILE: u8 = 2;
const END: u8 = 0;
const MAX_PATH: usize = 4096;

fn invalid(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "deadline")
}

/// The archive path of `path` under `root` ('/'-separated).
pub fn relative(root: &Path, path: &Path) -> io::Result<String> {
    let rel = path
        .strip_prefix(root)
        .map_err(|_| invalid("outside_root"))?;
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str().ok_or_else(|| invalid("not_utf8"))?),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => return Err(invalid("path_component")),
        }
    }
    Ok(parts.join("/"))
}

/// A checked archive path: components that are not empty, `.`, `..`, and
/// hold no separator or NUL.
pub fn checked(path: &str) -> io::Result<PathBuf> {
    if path.is_empty() || path.len() > MAX_PATH {
        return Err(invalid("path_length"));
    }
    let mut out = PathBuf::new();
    for part in path.split('/') {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.contains('\\')
            || part.contains('\0')
            || part.contains(':')
        {
            return Err(invalid("path_component"));
        }
        out.push(part);
    }
    Ok(out)
}

pub struct ArchiveWriter<W: Write> {
    inner: W,
    /// Bytes of file content written.
    pub bytes: u64,
}

impl<W: Write> ArchiveWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner, bytes: 0 }
    }

    fn path(&mut self, path: &str) -> io::Result<()> {
        checked(path)?;
        let len = u16::try_from(path.len()).map_err(|_| invalid("path_length"))?;
        self.inner.write_all(&len.to_be_bytes())?;
        self.inner.write_all(path.as_bytes())
    }

    pub fn dir(&mut self, path: &str) -> io::Result<()> {
        self.inner.write_all(&[DIR])?;
        self.path(path)
    }

    pub fn file(&mut self, path: &str, source: &Path) -> io::Result<()> {
        let mut f = fs::File::open(source)?;
        let size = f.metadata()?.len();
        self.inner.write_all(&[FILE])?;
        self.path(path)?;
        self.inner.write_all(&size.to_be_bytes())?;
        // Exactly `size` bytes, even if the file changed meanwhile.
        let copied = io::copy(&mut (&mut f).take(size), &mut self.inner)?;
        if copied != size {
            return Err(invalid("file_shrank"));
        }
        self.bytes = self.bytes.saturating_add(size);
        Ok(())
    }

    /// Every directory and file under `dir` (sorted, depth first), with
    /// archive paths relative to `root`.
    pub fn tree(&mut self, root: &Path, dir: &Path, deadline: Instant) -> io::Result<()> {
        if Instant::now() > deadline {
            return Err(timed_out());
        }
        self.dir(&relative(root, dir)?)?;
        let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let ty = e.file_type()?;
            let path = e.path();
            if ty.is_dir() {
                self.tree(root, &path, deadline)?;
            } else if ty.is_file() {
                self.file(&relative(root, &path)?, &path)?;
            } else {
                return Err(not_regular(&path));
            }
        }
        Ok(())
    }

    pub fn end(mut self) -> io::Result<W> {
        self.inner.write_all(&[END])?;
        Ok(self.inner)
    }
}

/// One archive entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Dir(String),
    /// A file of `size` bytes: read them with [`ArchiveReader::content`].
    File(String, u64),
}

pub struct ArchiveReader<R: Read> {
    inner: R,
    ended: bool,
}

impl<R: Read> ArchiveReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            ended: false,
        }
    }

    fn path(&mut self) -> io::Result<String> {
        let mut len = [0u8; 2];
        self.inner.read_exact(&mut len)?;
        let mut buf = vec![0u8; usize::from(u16::from_be_bytes(len))];
        self.inner.read_exact(&mut buf)?;
        let path = String::from_utf8(buf).map_err(|_| invalid("not_utf8"))?;
        checked(&path)?;
        Ok(path)
    }

    /// The next entry; `None` at the end marker (which must be the last
    /// byte of the stream).
    pub fn next_entry(&mut self) -> io::Result<Option<Entry>> {
        if self.ended {
            return Ok(None);
        }
        let mut tag = [0u8; 1];
        self.inner.read_exact(&mut tag)?;
        match tag[0] {
            DIR => Ok(Some(Entry::Dir(self.path()?))),
            FILE => {
                let path = self.path()?;
                let mut size = [0u8; 8];
                self.inner.read_exact(&mut size)?;
                Ok(Some(Entry::File(path, u64::from_be_bytes(size))))
            }
            END => {
                let mut extra = [0u8; 1];
                if self.inner.read(&mut extra)? != 0 {
                    return Err(invalid("data_after_end"));
                }
                self.ended = true;
                Ok(None)
            }
            _ => Err(invalid("entry_tag")),
        }
    }

    /// Copy the content of the file entry just read into `out`.
    pub fn content(&mut self, size: u64, out: &mut impl Write) -> io::Result<()> {
        let copied = io::copy(&mut (&mut self.inner).take(size), out)?;
        if copied != size {
            return Err(invalid("truncated_file"));
        }
        Ok(())
    }
}

/// Whether a repository file is left out of a copy.
fn skipped(name: &str) -> bool {
    name.ends_with(".lock")
        || name == "gc.pid"
        || name.starts_with("tmp_objdir-incoming-")
        || name.starts_with("incoming-")
        || name.starts_with("tmp_pack_")
        || name.starts_with("tmp_idx_")
        // Derived files the next maintenance rebuilds (commit-graph write):
        // a copy taken while one is rewritten could be torn.
        || name == "commit-graph"
        || name == "commit-graphs"
        || name.starts_with("multi-pack-index")
}

/// Not a directory or a regular file (a symlink, a socket): named by its
/// path, so the repository and the entry are in the error.
fn not_regular(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("not_a_regular_file: {}", path.display()),
    )
}

fn copy_tree(src: &Path, dst: &Path, deadline: Instant) -> io::Result<()> {
    if Instant::now() > deadline {
        return Err(timed_out());
    }
    fs::create_dir_all(dst)?;
    let mut entries: Vec<_> = fs::read_dir(src)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name();
        let Some(text) = name.to_str() else {
            return Err(invalid("not_utf8"));
        };
        if skipped(text) {
            continue;
        }
        let ty = e.file_type()?;
        let to = dst.join(&name);
        if ty.is_dir() {
            copy_tree(&e.path(), &to, deadline)?;
        } else if ty.is_file() {
            match fs::copy(e.path(), &to) {
                Ok(_) => {}
                // A file removed while the copy ran (a loose ref packed, a
                // loose object packed and pruned): it is in packed-refs or a
                // pack copied later, or no longer needed.
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        } else {
            return Err(not_regular(&e.path()));
        }
    }
    Ok(())
}

/// Copy the bare repository `src` to `dst` (which must not exist), refs
/// before objects (module docs).
pub fn copy_repository(src: &Path, dst: &Path, deadline: Instant) -> io::Result<()> {
    if dst.exists() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "copy_target"));
    }
    fs::create_dir_all(dst)?;
    let refs = src.join("refs");
    if refs.is_dir() {
        copy_tree(&refs, &dst.join("refs"), deadline)?;
    }
    let mut top: Vec<_> = fs::read_dir(src)?.collect::<Result<_, _>>()?;
    top.sort_by_key(|e| {
        // packed-refs first (right after the loose refs), objects last.
        let name = e.file_name();
        match name.to_str() {
            Some("packed-refs") => (0, name),
            Some("objects") => (2, name),
            _ => (1, name),
        }
    });
    for e in top {
        let name = e.file_name();
        let Some(text) = name.to_str() else {
            return Err(invalid("not_utf8"));
        };
        if text == "refs" || skipped(text) {
            continue;
        }
        let ty = e.file_type()?;
        if ty.is_dir() {
            copy_tree(&e.path(), &dst.join(&name), deadline)?;
        } else if ty.is_file() {
            match fs::copy(e.path(), dst.join(&name)) {
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        } else {
            return Err(not_regular(&e.path()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_that_could_escape_are_refused() {
        for bad in ["", "/abs", "a/../b", "a//b", "./a", "a\\b", "c:x", "a/\0"] {
            assert!(checked(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            checked("repos/ab/1.git/HEAD").expect("ok"),
            PathBuf::from("repos").join("ab").join("1.git").join("HEAD")
        );
    }

    #[test]
    fn an_archive_round_trips_and_ends_once() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path();
        fs::create_dir_all(root.join("repos/ab/1.git/refs/heads")).expect("dirs");
        fs::write(root.join("repos/ab/1.git/HEAD"), b"ref: refs/heads/main\n").expect("head");
        fs::write(root.join("repos/ab/1.git/refs/heads/main"), b"abc\n").expect("ref");
        let mut w = ArchiveWriter::new(Vec::new());
        w.tree(
            root,
            &root.join("repos/ab/1.git"),
            Instant::now() + std::time::Duration::from_secs(10),
        )
        .expect("tree");
        let bytes = w.end().expect("end");
        let mut r = ArchiveReader::new(bytes.as_slice());
        let mut seen = Vec::new();
        while let Some(e) = r.next_entry().expect("entry") {
            if let Entry::File(_, size) = &e {
                let mut out = Vec::new();
                r.content(*size, &mut out).expect("content");
            }
            seen.push(e);
        }
        assert_eq!(seen[0], Entry::Dir("repos/ab/1.git".to_owned()));
        assert!(seen.contains(&Entry::File("repos/ab/1.git/HEAD".to_owned(), 21)));
        assert!(seen.contains(&Entry::File("repos/ab/1.git/refs/heads/main".to_owned(), 4)));
        let mut longer = bytes.clone();
        longer.push(9);
        let mut r = ArchiveReader::new(longer.as_slice());
        let mut err = None;
        loop {
            match r.next_entry() {
                Ok(Some(Entry::File(_, size))) => r.content(size, &mut io::sink()).expect("c"),
                Ok(Some(Entry::Dir(_))) => {}
                Ok(None) => break,
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        assert!(err.is_some(), "data after the end marker is refused");
    }
}
