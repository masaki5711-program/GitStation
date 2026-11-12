//! `gstn pipeline artifact-download` (M4 C6): one CI artifact into a new
//! file. The job's artifact list is read first (its size and sha256: the
//! same decision on the marks as the download, `FG-AUTH-0022` for an AI
//! agent's token unless both marks are open), then the bytes are written to
//! `--output` (created new: never over an existing file), bounded by the
//! listed size, and their sha256 compared with the listed one. A mismatch
//! removes the file (`FG-CLI-0003 reason=sha256_mismatch`). Tests:
//! `tests/pipeline_artifacts.rs`.

use std::io::Write;
use std::path::Path;

use forge_errors::ErrorCode;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::app::{emit, EnvFn, Io};
use crate::cli::Cli;
use crate::error::{ClientError, Result};
use crate::hosting::{call, client};
use crate::http::Method;

/// How long one download may take.
const DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1800);

/// Writes to the file and hashes what it writes.
struct Hashing {
    file: std::fs::File,
    hasher: Sha256,
}

impl Write for Hashing {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.file.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

fn output_failed(reason: &str) -> ClientError {
    ClientError::local(ErrorCode::CliOutputFailed, format!("reason={reason}"))
}

/// `(project, pipeline, job, artifact)` into `output` (module docs).
pub fn download(
    cli: &Cli,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
    (project, pipeline, job, id): (i64, i64, i64, i64),
    output: &Path,
) -> Result<()> {
    let c = client(cli, env, io)?;
    let base = format!("/api/v1/projects/{project}/pipelines/{pipeline}/jobs/{job}/artifacts");
    let list = call(&c, Method::Get, &base, &[], None)?;
    let listed = list
        .get("artifacts")
        .and_then(Value::as_array)
        .and_then(|a| {
            a.iter()
                .find(|x| x.get("id").and_then(Value::as_i64) == Some(id))
        })
        .ok_or_else(|| {
            ClientError::local(ErrorCode::CliUsageInvalid, "reason=artifact_not_listed")
        })?;
    let size = listed
        .get("size_bytes")
        .and_then(Value::as_u64)
        .ok_or_else(|| ClientError::local(ErrorCode::CliResponseInvalid, "reason=size_missing"))?;
    let want = listed
        .get("sha256")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|_| output_failed("output_exists_or_unwritable"))?;
    let mut sink = Hashing {
        file,
        hasher: Sha256::new(),
    };
    let got = c.download_to(
        &format!("{base}/{id}"),
        None,
        &mut sink,
        size,
        DOWNLOAD_TIMEOUT,
    );
    let digest: String = sink
        .hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let checked = match got {
        Ok(n) if n == size && digest == want => Ok(n),
        Ok(_) => Err(output_failed("sha256_mismatch")),
        Err(e) => Err(e),
    };
    let written = match checked {
        Ok(n) => n,
        Err(e) => {
            // Never leave a partial or wrong file behind.
            if std::fs::remove_file(output).is_err() {
                return Err(output_failed("partial_file_left"));
            }
            return Err(e);
        }
    };
    let out = json!({
        "id": id,
        "output": output.display().to_string(),
        "size_bytes": written,
        "sha256": digest,
    });
    let human = format!(
        "Wrote {} ({written} bytes, sha256 {digest})",
        output.display()
    );
    emit(io, cli, out, &human)
}
