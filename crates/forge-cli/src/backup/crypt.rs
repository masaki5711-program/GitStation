//! The encrypted stream of a backup file (M3 O1): AES-256-GCM in chunks
//! (the STREAM construction), so a repository archive of any size is
//! written and read in bounded memory, and truncation, reordering or a
//! changed byte is detected.
//!
//! ```text
//! header  = "FGBK" | version (1) | kind (1) | nonce prefix (7 random bytes)
//! frame   = length of the ciphertext (u32, big endian) | ciphertext
//! nonce   = nonce prefix | frame counter (u32, big endian) | last (0 or 1)
//! aad     = header | context (the generation name for an archive, `state` for a
//!           state copy: a file moved to another generation does not open)
//! ```
//!
//! Every plaintext chunk is at most [`CHUNK`] bytes; the last frame (maybe
//! empty) carries `last = 1`, and nothing may follow it. The key is
//! `backup.key_file` (forge-core `BackupKey`).

use std::io::{self, Read, Write};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use forge_core::config::BackupKey;

/// The plaintext bytes of one frame.
pub const CHUNK: usize = 1 << 20;
const TAG: usize = 16;
const MAGIC: &[u8; 4] = b"FGBK";
const VERSION: u8 = 1;
const PREFIX: usize = 7;
pub const HEADER_LEN: usize = 4 + 1 + 1 + PREFIX;

/// What a file holds (part of the authenticated header: a state copy can
/// never be read as a repository archive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Repositories = 1,
    StateCopy = 2,
}

fn invalid(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

fn cipher(key: &BackupKey) -> io::Result<Aes256Gcm> {
    Aes256Gcm::new_from_slice(key.expose()).map_err(|_| invalid("key_length"))
}

fn nonce(prefix: &[u8; PREFIX], counter: u32, last: bool) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..PREFIX].copy_from_slice(prefix);
    n[PREFIX..PREFIX + 4].copy_from_slice(&counter.to_be_bytes());
    n[11] = u8::from(last);
    n
}

/// Writes the encrypted stream; [`SealWriter::finish`] writes the last
/// frame (dropping the writer without it leaves a file every reader
/// refuses as truncated).
pub struct SealWriter<W: Write> {
    inner: W,
    cipher: Aes256Gcm,
    aad: Vec<u8>,
    prefix: [u8; PREFIX],
    counter: u32,
    buf: Vec<u8>,
}

impl<W: Write> SealWriter<W> {
    pub fn new(mut inner: W, key: &BackupKey, kind: Kind, context: &[u8]) -> io::Result<Self> {
        let mut prefix = [0u8; PREFIX];
        getrandom::getrandom(&mut prefix).map_err(|_| io::Error::other("rng_unavailable"))?;
        let mut header = [0u8; HEADER_LEN];
        header[..4].copy_from_slice(MAGIC);
        header[4] = VERSION;
        header[5] = kind as u8;
        header[6..].copy_from_slice(&prefix);
        inner.write_all(&header)?;
        Ok(Self {
            inner,
            cipher: cipher(key)?,
            aad: [header.as_slice(), context].concat(),
            prefix,
            counter: 0,
            buf: Vec::with_capacity(CHUNK),
        })
    }

    fn frame(&mut self, last: bool) -> io::Result<()> {
        let n = nonce(&self.prefix, self.counter, last);
        let sealed = self
            .cipher
            .encrypt(
                Nonce::from_slice(&n),
                Payload {
                    msg: &self.buf,
                    aad: &self.aad,
                },
            )
            .map_err(|_| io::Error::other("encrypt"))?;
        let len = u32::try_from(sealed.len()).map_err(|_| invalid("frame_too_large"))?;
        self.inner.write_all(&len.to_be_bytes())?;
        self.inner.write_all(&sealed)?;
        self.buf.clear();
        self.counter = self
            .counter
            .checked_add(1)
            .ok_or_else(|| invalid("too_many_frames"))?;
        Ok(())
    }

    /// Write the last frame and flush; returns the inner writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.frame(true)?;
        self.inner.flush()?;
        Ok(self.inner)
    }
}

impl<W: Write> Write for SealWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let room = CHUNK - self.buf.len();
        let take = room.min(data.len());
        self.buf.extend_from_slice(&data[..take]);
        if self.buf.len() == CHUNK {
            self.frame(false)?;
        }
        Ok(take)
    }

    fn flush(&mut self) -> io::Result<()> {
        // Frames are written whole; the inner writer is flushed by finish.
        Ok(())
    }
}

/// Reads and checks the encrypted stream. Every error is
/// `InvalidData` (a wrong key, a changed byte, a truncated file, another
/// kind) or the inner reader's.
pub struct OpenReader<R: Read> {
    inner: R,
    cipher: Aes256Gcm,
    aad: Vec<u8>,
    prefix: [u8; PREFIX],
    counter: u32,
    plain: Vec<u8>,
    pos: usize,
    done: bool,
}

impl<R: Read> OpenReader<R> {
    pub fn new(mut inner: R, key: &BackupKey, kind: Kind, context: &[u8]) -> io::Result<Self> {
        let mut header = [0u8; HEADER_LEN];
        inner
            .read_exact(&mut header)
            .map_err(|_| invalid("header"))?;
        if &header[..4] != MAGIC || header[4] != VERSION {
            return Err(invalid("not_a_backup_file"));
        }
        if header[5] != kind as u8 {
            return Err(invalid("wrong_kind"));
        }
        let mut prefix = [0u8; PREFIX];
        prefix.copy_from_slice(&header[6..]);
        Ok(Self {
            inner,
            cipher: cipher(key)?,
            aad: [header.as_slice(), context].concat(),
            prefix,
            counter: 0,
            plain: Vec::new(),
            pos: 0,
            done: false,
        })
    }

    fn next_frame(&mut self) -> io::Result<()> {
        let mut len = [0u8; 4];
        self.inner
            .read_exact(&mut len)
            .map_err(|_| invalid("truncated"))?;
        let len = usize::try_from(u32::from_be_bytes(len)).map_err(|_| invalid("frame"))?;
        if !(TAG..=CHUNK + TAG).contains(&len) {
            return Err(invalid("frame_length"));
        }
        let mut sealed = vec![0u8; len];
        self.inner
            .read_exact(&mut sealed)
            .map_err(|_| invalid("truncated"))?;
        let open = |last: bool| {
            let n = nonce(&self.prefix, self.counter, last);
            self.cipher.decrypt(
                Nonce::from_slice(&n),
                Payload {
                    msg: &sealed,
                    aad: &self.aad,
                },
            )
        };
        let (plain, last) = match open(false) {
            Ok(p) => (p, false),
            Err(_) => (open(true).map_err(|_| invalid("authentication"))?, true),
        };
        if !last && plain.len() != CHUNK {
            return Err(invalid("short_frame"));
        }
        if last {
            let mut extra = [0u8; 1];
            if self.inner.read(&mut extra)? != 0 {
                return Err(invalid("data_after_last_frame"));
            }
            self.done = true;
        }
        self.counter = self
            .counter
            .checked_add(1)
            .ok_or_else(|| invalid("too_many_frames"))?;
        self.plain = plain;
        self.pos = 0;
        Ok(())
    }
}

impl<R: Read> Read for OpenReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.plain.len() {
            if self.done {
                return Ok(0);
            }
            self.next_frame()?;
        }
        let n = out.len().min(self.plain.len() - self.pos);
        out[..n].copy_from_slice(&self.plain[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(b: u8) -> BackupKey {
        BackupKey::from_bytes([b; 32])
    }

    fn seal(data: &[u8], kind: Kind) -> Vec<u8> {
        let mut w =
            SealWriter::new(Vec::new(), &key(7), kind, b"20261109T021500Z").expect("writer");
        w.write_all(data).expect("write");
        w.finish().expect("finish")
    }

    fn open(file: &[u8], k: &BackupKey, kind: Kind) -> io::Result<Vec<u8>> {
        let mut r = OpenReader::new(file, k, kind, b"20261109T021500Z")?;
        let mut out = Vec::new();
        r.read_to_end(&mut out)?;
        Ok(out)
    }

    #[test]
    fn round_trips_empty_small_and_multi_chunk_data() {
        for len in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 2 * CHUNK + 17] {
            let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let file = seal(&data, Kind::Repositories);
            assert_eq!(
                open(&file, &key(7), Kind::Repositories).expect("open"),
                data,
                "{len}"
            );
        }
    }

    #[test]
    fn a_wrong_key_a_changed_byte_truncation_and_another_kind_are_refused() {
        let data = vec![42u8; CHUNK + 5];
        let file = seal(&data, Kind::Repositories);
        let err = open(&file, &key(8), Kind::Repositories).expect_err("wrong key");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let mut changed = file.clone();
        let mid = changed.len() / 2;
        changed[mid] ^= 1;
        open(&changed, &key(7), Kind::Repositories).expect_err("changed byte");
        // Cut after the first (full) frame: the last frame is missing.
        let first = HEADER_LEN + 4 + CHUNK + TAG;
        open(&file[..first], &key(7), Kind::Repositories).expect_err("truncated");
        open(&file[..file.len() - 1], &key(7), Kind::Repositories).expect_err("cut");
        let mut longer = file.clone();
        longer.push(0);
        open(&longer, &key(7), Kind::Repositories).expect_err("trailing data");
        open(&file, &key(7), Kind::StateCopy).expect_err("another kind");
        // Moved to another generation: the context is authenticated too.
        let mut r = OpenReader::new(
            file.as_slice(),
            &key(7),
            Kind::Repositories,
            b"20261110T021500Z",
        )
        .expect("header");
        let mut out = Vec::new();
        r.read_to_end(&mut out).expect_err("another generation");
    }

    #[test]
    fn the_plaintext_never_appears_in_the_file() {
        let secret = b"synthetic-secret-value-0123456789";
        let file = seal(secret, Kind::StateCopy);
        assert!(!file.windows(secret.len()).any(|w| w == secret));
    }
}
