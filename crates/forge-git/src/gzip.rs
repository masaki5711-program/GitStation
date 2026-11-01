//! Decoding `Content-Encoding: gzip` request bodies with a decompressed-size
//! cap. git compresses fetch negotiation bodies; a small compressed body can
//! expand without bound, so decoding stops at the cap (FG-GIT-0013) instead of
//! materialising the output first.

use std::io::{self, Write};

use flate2::write::GzDecoder;
use forge_errors::{ErrorCode, ForgeError};

/// Collects decompressed bytes and refuses to grow past `cap`.
#[derive(Debug)]
struct CappedSink {
    out: Vec<u8>,
    total: usize,
    cap: usize,
    over: bool,
}

impl Write for CappedSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.total.saturating_add(buf.len()) > self.cap {
            self.over = true;
            return Err(io::Error::other("decompressed size cap"));
        }
        self.total += buf.len();
        self.out.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Incremental gzip decoder: feed compressed chunks as they arrive, take the
/// decompressed bytes out as you go.
#[derive(Debug)]
pub struct GzipDecoder {
    inner: GzDecoder<CappedSink>,
    cap: usize,
}

impl GzipDecoder {
    /// `cap`: the most decompressed bytes accepted in total.
    pub fn new(cap: usize) -> Self {
        Self {
            inner: GzDecoder::new(CappedSink {
                out: Vec::new(),
                total: 0,
                cap,
                over: false,
            }),
            cap,
        }
    }

    fn map_err(&self, e: &io::Error) -> ForgeError {
        if self.inner.get_ref().over {
            ForgeError::new(ErrorCode::GitRequestTooLarge)
                .with_context("limit", "gzip_decompressed")
                .with_context("max_bytes", self.cap)
        } else {
            ForgeError::new(ErrorCode::GitProtocolInvalid)
                .with_context("reason", "gzip")
                .with_context("detail", e.kind())
        }
    }

    /// Feed one compressed chunk; returns the bytes decompressed so far that
    /// were not returned before.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>, ForgeError> {
        let mut rest = chunk;
        while !rest.is_empty() {
            match self.inner.write(rest) {
                // 0: the gzip member already ended; anything after it is junk.
                Ok(0) => {
                    return Err(ForgeError::new(ErrorCode::GitProtocolInvalid)
                        .with_context("reason", "gzip")
                        .with_context("detail", "trailing_data"))
                }
                Ok(n) => rest = rest.get(n..).unwrap_or_default(),
                Err(e) => return Err(self.map_err(&e)),
            }
        }
        Ok(std::mem::take(&mut self.inner.get_mut().out))
    }

    /// The body ended: check that the gzip stream was complete and return
    /// the remaining bytes.
    pub fn finish(mut self) -> Result<Vec<u8>, ForgeError> {
        if let Err(e) = self.inner.try_finish() {
            return Err(self.map_err(&e));
        }
        match self.inner.finish() {
            Ok(sink) => Ok(sink.out),
            Err(e) => Err(ForgeError::new(ErrorCode::GitProtocolInvalid)
                .with_context("reason", "gzip")
                .with_context("detail", e.kind())),
        }
    }
}

/// Decode a whole gzip body with a decompressed-size cap.
pub fn decode_capped(body: &[u8], cap: usize) -> Result<Vec<u8>, ForgeError> {
    let mut d = GzipDecoder::new(cap);
    let mut out = d.push(body)?;
    out.extend(d.finish()?);
    Ok(out)
}

#[cfg(test)]
mod tests {

    use flate2::write::GzEncoder;
    use flate2::Compression;

    use super::*;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut e = GzEncoder::new(Vec::new(), Compression::best());
        e.write_all(data).expect("compress");
        e.finish().expect("finish")
    }

    #[test]
    fn round_trip_under_the_cap() {
        let body = b"0032want 0123456789012345678901234567890123456789\n0000".repeat(50);
        assert_eq!(decode_capped(&gzip(&body), 1 << 20).expect("decode"), body);
    }

    #[test]
    fn exactly_the_cap_is_accepted() {
        let body = vec![b'x'; 4096];
        assert_eq!(
            decode_capped(&gzip(&body), 4096).expect("decode").len(),
            4096
        );
    }

    /// 256 MiB of zeros compress to ~250 KiB; decoding must stop at the
    /// 1 MiB cap, not expand everything first.
    #[test]
    fn gzip_bomb_stops_at_cap() {
        let mut e = GzEncoder::new(Vec::new(), Compression::best());
        let zeros = vec![0u8; 1 << 20];
        for _ in 0..256 {
            e.write_all(&zeros).expect("compress");
        }
        let bomb = e.finish().expect("finish");
        assert!(bomb.len() < 1 << 20, "the bomb is small: {}", bomb.len());

        let cap = 1 << 20;
        let mut d = GzipDecoder::new(cap);
        let mut seen = 0usize;
        let mut err = None;
        for chunk in bomb.chunks(16 * 1024) {
            match d.push(chunk) {
                Ok(out) => seen += out.len(),
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        let err = err.expect("the cap is hit");
        assert_eq!(err.code(), ErrorCode::GitRequestTooLarge);
        assert!(err.to_string().contains("limit=gzip_decompressed"), "{err}");
        assert!(seen <= cap, "never more than the cap is produced: {seen}");
        assert!(d.inner.get_ref().total <= cap);
    }

    #[test]
    fn corrupt_gzip_is_protocol_invalid() {
        let err = decode_capped(b"this is not gzip at all", 1024).expect_err("corrupt");
        assert_eq!(err.code(), ErrorCode::GitProtocolInvalid);
        assert!(err.to_string().contains("reason=gzip"), "{err}");
    }

    #[test]
    fn truncated_gzip_is_protocol_invalid() {
        let full = gzip(&vec![b'a'; 10_000]);
        let err = decode_capped(full.get(..full.len() / 2).expect("half"), 1 << 20)
            .expect_err("truncated");
        assert_eq!(err.code(), ErrorCode::GitProtocolInvalid);
    }

    #[test]
    fn data_after_the_gzip_member_is_rejected() {
        let mut body = gzip(b"0000");
        body.extend_from_slice(b"junk");
        let err = decode_capped(&body, 1024).expect_err("trailing");
        assert_eq!(err.code(), ErrorCode::GitProtocolInvalid);
    }
}
