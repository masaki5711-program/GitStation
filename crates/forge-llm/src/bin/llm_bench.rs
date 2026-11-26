//! `llm-bench`: run the LLM eval sets against an Ollama and write the
//! ADR-0034 `eval-report.json` (plan 19 §6, L5-10; [`forge_llm::bench`]).
//!
//! ```text
//! llm-bench --endpoint http://127.0.0.1:11434 --out eval-report.json
//!           [--evals <dir>] [--runs <1-15>] [--allow-host <host>]... [--ca-file <pem>]
//! ```
//!
//! - The model is `evals/bench.toml`'s `model` (`name:tag@sha256:…`): its
//!   digest is checked on the endpoint first (`FG-LLM-0003` otherwise, no
//!   report).
//! - The endpoint is loopback or a host given with `--allow-host` (in-house
//!   only, contract 8; `FG-SYS-0002 reason=llm_endpoint_not_internal`).
//! - Exit 0: the report is written (whatever the cases scored: the gate is
//!   `forge/ai-eval`). Exit 1: no report — the failure is printed once
//!   with its code (`error_code=… key=value …`).
//!
//! The job's stderr carries a one-line summary per purpose; the report
//! itself has only the v1 fields.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use forge_core::config::llm::is_loopback;
use forge_errors::{ErrorCode, ForgeError};
use forge_llm::bench::{self, bench_invalid, BenchConfig};
use forge_llm::ollama::OllamaProvider;
use forge_llm::ModelCatalog;

#[derive(Debug, Default)]
struct Args {
    endpoint: Option<String>,
    out: Option<PathBuf>,
    evals: Option<PathBuf>,
    runs: Option<u32>,
    allow_hosts: Vec<String>,
    ca_file: Option<PathBuf>,
}

fn parse_args(mut argv: impl Iterator<Item = String>) -> Result<Args, ForgeError> {
    let mut a = Args::default();
    while let Some(flag) = argv.next() {
        let mut value = || {
            argv.next()
                .ok_or_else(|| bench_invalid("args", "missing", flag.clone()))
        };
        match flag.as_str() {
            "--endpoint" => a.endpoint = Some(value()?),
            "--out" => a.out = Some(PathBuf::from(value()?)),
            "--evals" => a.evals = Some(PathBuf::from(value()?)),
            "--runs" => {
                let v = value()?;
                let n: u32 = v
                    .parse()
                    .map_err(|_| bench_invalid("args", "invalid", "--runs"))?;
                if !(1..=forge_core::ai_eval::MAX_RUNS).contains(&n) {
                    return Err(bench_invalid("args", "invalid", "--runs"));
                }
                a.runs = Some(n);
            }
            "--allow-host" => a.allow_hosts.push(value()?.to_ascii_lowercase()),
            "--ca-file" => a.ca_file = Some(PathBuf::from(value()?)),
            other => return Err(bench_invalid("args", "unknown", other.to_owned())),
        }
    }
    Ok(a)
}

/// `http(s)://host[:port]` with no path; the host loopback or allowed.
fn check_endpoint(raw: &str, allowed: &[String], ca: bool) -> Result<String, ForgeError> {
    let bad = |reason| bench_invalid("endpoint", reason, "--endpoint");
    let trimmed = raw.strip_suffix('/').unwrap_or(raw);
    let (scheme, rest) = trimmed.split_once("://").ok_or_else(|| bad("invalid"))?;
    if scheme != "http" && scheme != "https" {
        return Err(bad("invalid"));
    }
    if rest.is_empty() || rest.contains(['/', '?', '#', '@']) {
        return Err(bad("invalid"));
    }
    let host = if rest.starts_with('[') {
        rest.split_once(']').map(|(h, _)| format!("{h}]"))
    } else {
        rest.split(':').next().map(str::to_owned)
    }
    .ok_or_else(|| bad("invalid"))?;
    let host = host.to_ascii_lowercase();
    if !is_loopback(&host) && !allowed.contains(&host) {
        return Err(bad("llm_endpoint_not_internal"));
    }
    if scheme == "https" && !ca {
        return Err(bench_invalid("ca_file", "missing", "--ca-file"));
    }
    Ok(trimmed.to_owned())
}

fn default_evals() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("evals")
}

async fn bench_main(args: Args) -> Result<String, ForgeError> {
    let endpoint = args
        .endpoint
        .as_deref()
        .ok_or_else(|| bench_invalid("args", "missing", "--endpoint"))?;
    let out = args
        .out
        .ok_or_else(|| bench_invalid("args", "missing", "--out"))?;
    let endpoint = check_endpoint(endpoint, &args.allow_hosts, args.ca_file.is_some())?;
    let evals = args.evals.unwrap_or_else(default_evals);
    let cfg_text = std::fs::read_to_string(evals.join(bench::BENCH_FILE))
        .map_err(|_| bench_invalid("config", "unreadable", bench::BENCH_FILE))?;
    let (mut cfg, model) = BenchConfig::parse(&cfg_text)?;
    if let Some(runs) = args.runs {
        cfg.runs = runs;
    }
    let cases = bench::load_sets(&evals)?;
    let ca = match &args.ca_file {
        Some(p) => Some(
            std::fs::read(p).map_err(|_| bench_invalid("ca_file", "unreadable", "--ca-file"))?,
        ),
        None => None,
    };
    let provider = Arc::new(OllamaProvider::new(
        &endpoint,
        ca.as_deref(),
        Duration::from_millis(cfg.timeout_ms),
    )?);
    // The digest first: a report of another model is worse than none.
    match provider.digest(&model.name).await? {
        Some(d) if model.digest_matches(&d) => {}
        Some(_) => {
            return Err(ForgeError::new(ErrorCode::LlmModelDigestMismatch)
                .with_context("model", model.name.clone()) // clone: context owns it
                .with_context("reason", "digest_mismatch"));
        }
        None => {
            return Err(ForgeError::new(ErrorCode::LlmModelDigestMismatch)
                .with_context("model", model.name.clone()) // clone: as above
                .with_context("reason", "model_missing"));
        }
    }
    let result = bench::run(provider.as_ref(), &cfg, &model, &cases).await?;
    let bytes = bench::render_report(&result.report)?;
    std::fs::write(&out, &bytes)
        .map_err(|_| bench_invalid("out", "unwritable", out.display().to_string()))?;

    let mut lines = Vec::new();
    for c in &result.cases {
        let invalid = if c.invalid_runs > 0 {
            format!(" (invalid answers: {})", c.invalid_runs)
        } else {
            String::new()
        };
        lines.push(format!(
            "{:<12} {}/{} {}{invalid}",
            c.kind.as_str(),
            c.passes,
            c.runs,
            c.id
        ));
    }
    for (purpose, (n, all)) in bench::safety_counts(&result) {
        lines.push(format!(
            "safety {purpose}: {all}/{n} cases passed every run"
        ));
    }
    // The gate is forge/ai-eval (the job still exits 0 with a report), but a
    // failed safety case must stand out in the job log.
    for c in result
        .cases
        .iter()
        .filter(|c| c.kind == forge_core::ai_eval::CaseKind::Safety && c.passes != c.runs)
    {
        lines.push(format!(
            "UNSAFE: {} passed {}/{} runs",
            c.id, c.passes, c.runs
        ));
    }
    for p in &result.skipped {
        lines.push(format!("skipped {p}: no prompt in use yet"));
    }
    let m = result.report.metrics;
    lines.push(format!(
        "metrics cause_hit_rate={:.3} attach_precision={:.3} false_alarm_rate={:.3} \
         abstain_accuracy={:.3} p95_latency_ms={:.0}",
        m.cause_hit_rate,
        m.attach_precision,
        m.false_alarm_rate,
        m.abstain_accuracy,
        m.p95_latency_ms
    ));
    if !result.empty_metrics.is_empty() {
        lines.push(format!(
            "nothing to count (reported at the best value): {}",
            result.empty_metrics.join(", ")
        ));
    }
    lines.push(format!(
        "written {} (models {}, {} cases)",
        out.display(),
        result.report.models,
        result.report.cases.len()
    ));
    Ok(lines.join("\n"))
}

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => {
            eprintln!("llm-bench: error_code=FG-SYS-0001 reason=runtime_start");
            return ExitCode::FAILURE;
        }
    };
    let outcome =
        parse_args(std::env::args().skip(1)).map(|args| runtime.block_on(bench_main(args)));
    match outcome.and_then(|r| r) {
        Ok(summary) => {
            eprintln!("{summary}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            let context = e
                .context()
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ");
            eprintln!("llm-bench: error_code={} {context}", e.code().id());
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_endpoint_is_loopback_or_allowed_and_has_no_path() {
        assert_eq!(
            check_endpoint("http://127.0.0.1:11434/", &[], false).expect("loopback"),
            "http://127.0.0.1:11434"
        );
        assert!(check_endpoint("http://[::1]:11434", &[], false).is_ok());
        let allowed = vec!["gpu1.corp.test".to_owned()];
        assert!(check_endpoint("http://gpu1.corp.test:11434", &allowed, false).is_ok());
        for (raw, reason) in [
            (
                "http://ollama.example.com:11434",
                "llm_endpoint_not_internal",
            ),
            ("http://127.0.0.1.example.com", "llm_endpoint_not_internal"),
            ("http://127.0.0.1:11434/api", "invalid"),
            ("http://user@127.0.0.1", "invalid"),
            ("ftp://127.0.0.1", "invalid"),
            ("127.0.0.1:11434", "invalid"),
        ] {
            let e = check_endpoint(raw, &allowed, false).expect_err(raw);
            assert_eq!(e.code(), ErrorCode::ConfigInvalid, "{raw}");
            assert!(
                e.context()
                    .iter()
                    .any(|(k, v)| *k == "reason" && v == reason),
                "{raw}: {:?}",
                e.context()
            );
        }
        let e = check_endpoint("https://127.0.0.1", &[], false).expect_err("no ca");
        assert!(e
            .context()
            .iter()
            .any(|(k, v)| *k == "config_key" && v == "llm_bench.ca_file"));
    }

    #[test]
    fn arguments_are_checked() {
        let args = |v: &[&str]| parse_args(v.iter().map(|s| (*s).to_owned()));
        let a = args(&[
            "--endpoint",
            "http://127.0.0.1:1",
            "--out",
            "r.json",
            "--runs",
            "3",
        ])
        .expect("valid");
        assert_eq!(a.runs, Some(3));
        assert!(args(&["--runs", "0"]).is_err());
        assert!(args(&["--runs", "16"]).is_err());
        assert!(args(&["--out"]).is_err());
        assert!(args(&["--verbose"]).is_err());
    }
}
