//! `forge diagnose` — the incident first diagnosis (plan 19 §3.2, M5 L5-7,
//! W14-5). An operator command on the server: it reads, it never changes
//! anything outside the LLM's own records.
//!
//! 1. **Facts, gathered by code** (`forge_core::llm::diagnosis`):
//!    - the log lines about the `--request-id` / `--error-code`, read from
//!      `--logs <file>` or stdin (`--logs -`). forge logs JSON to stderr
//!      and systemd keeps it in **journald**; there is no log file and no
//!      `forge diag bundle` yet (ADR-0039 Amendment L5-7), so the operator
//!      pipes the window: `journalctl -u forge-serve -u forge-worker
//!      --since -2h -o cat | forge diagnose --request-id <id> --logs -`;
//!    - the catalog entry of each code (the given one, or up to three the
//!      request's lines carry), its runbook section (`--runbooks`, else the
//!      release's `share/runbooks`) and the incident-memory records naming
//!      it (`--incidents`, else `share/incidents/records.jsonl`);
//!    - the deployments and migrations of `--since` (default 2 h) read as
//!      `forge_ro` in a read-only transaction
//!      (`forge_db::llm::diagnosis::recent_changes`), and the configuration
//!      file's modification time;
//!    - `forge doctor --json` (skipped with `--no-doctor`).
//!
//!    A source that cannot be read is a note in the output and
//!    `FG-SYS-0010` in the log (once per source); the diagnosis goes on.
//! 2. **Redaction first**: each part goes through `prepare` alone (a part
//!    whose secret cannot be redacted is withheld and noted), then all of
//!    them together. Only the prepared, redacted text is printed or sent.
//! 3. **The advice** (with `llm.enabled` and without `--facts-only`): a
//!    gateway of this process (no background task: an interactive request
//!    is never held for Kanflow or the bench) checks the model digest and
//!    asks with the `incident_diagnosis` prompt; the answer is checked by
//!    code — three candidates at most, every one citing a given log line or
//!    part, any step that changes something dropped (contract 7). The
//!    advice is written to `forge.llm_advice` (kind `incident_diagnosis`)
//!    by the gateway, which logs its own failures.
//! 4. **Output**: the advice (or why there is none, with its code) and,
//!    always, the fact bundle. The LLM being down, paused or off never
//!    stops the command (exit 0, the bundle printed); only an invalid
//!    configuration or target fails it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use forge_core::config::{self, Config};
use forge_core::llm::diagnosis::{self, CatalogFact, Facts, LogSelection, RunbookFact, Target};
use forge_core::llm::{prepare, InputClass, InputLimits, PreparedInput, Purpose, SubjectKind};
use forge_core::{telemetry, RequestId};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_llm::ollama::OllamaProvider;
use forge_llm::{Advised, DbRecords, Gateway, GatewayConfig, ModelCatalog, Outcome, Submission};
use serde_json::json;

use crate::runtime::block_on;

/// The most bytes read from the logs.
pub(crate) const MAX_LOG_BYTES: u64 = 64 * 1024 * 1024;
/// The most bytes read from one runbook or the incident memory.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// How long reading the logs may take (a pipe that never ends).
pub(crate) const LOG_READ_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the doctor's checks may take.
const DOCTOR_TIMEOUT: Duration = Duration::from_secs(120);
/// The longest window.
pub(crate) const MAX_SINCE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// What the operator asked for.
#[derive(Debug, Clone)]
pub(crate) struct Args {
    pub(crate) target: Target,
    pub(crate) since: Duration,
    /// `-`: stdin.
    pub(crate) logs: Option<PathBuf>,
    pub(crate) runbooks: Option<PathBuf>,
    pub(crate) incidents: Option<PathBuf>,
    pub(crate) doctor: bool,
    /// `false`: `--facts-only`, the LLM is not asked.
    pub(crate) ask: bool,
    pub(crate) json: bool,
}

/// A source that could not be read (`FG-SYS-0010`), or a part withheld.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Note {
    pub(crate) source: &'static str,
    pub(crate) reason: String,
    pub(crate) path: Option<String>,
}

impl Note {
    fn new(source: &'static str, reason: impl Into<String>, path: Option<&Path>) -> Self {
        Self {
            source,
            reason: reason.into(),
            path: path.map(|p| p.display().to_string()),
        }
    }

    fn error(&self) -> ForgeError {
        let mut e = ForgeError::new(ErrorCode::DiagnoseSourceUnreadable)
            .with_context("source", self.source)
            .with_context("reason", &self.reason);
        if let Some(p) = &self.path {
            e = e.with_context("path", p);
        }
        e
    }
}

/// Why there is no advice, or the advice.
#[derive(Debug)]
pub(crate) enum Ai {
    Advised(Box<Advised>),
    /// `--facts-only`.
    Skipped,
    /// `llm.enabled` is off (or the endpoint / model is not set).
    Off,
    /// The gateway is paused (`llm.paused`).
    Paused,
    /// Nothing to send (no fact at all).
    NoInput,
    /// The LLM (or its records) failed; already logged.
    Failed(ForgeError),
}

/// One diagnosis, ready to print.
#[derive(Debug)]
pub(crate) struct Report {
    pub(crate) target: Target,
    pub(crate) since: Duration,
    pub(crate) request_id: RequestId,
    pub(crate) input: Option<PreparedInput>,
    pub(crate) log: LogSelection,
    pub(crate) notes: Vec<Note>,
    pub(crate) ai: Ai,
    pub(crate) model: Option<String>,
}

/// `2h`, `30m`, `1d`, `90s` (at most 7 days).
pub(crate) fn parse_since(raw: &str) -> Result<Duration, String> {
    let raw = raw.trim();
    let (num, unit) = raw.split_at(raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len()));
    let n: u64 = num
        .parse()
        .map_err(|_| format!("{raw}: a number and a unit (s, m, h, d), e.g. 2h"))?;
    let secs = match unit {
        "s" => Some(n),
        "m" => n.checked_mul(60),
        "h" | "" => n.checked_mul(60 * 60),
        "d" => n.checked_mul(24 * 60 * 60),
        _ => None,
    }
    .ok_or_else(|| format!("{raw}: the unit is s, m, h or d"))?;
    let d = Duration::from_secs(secs);
    if d.is_zero() || d > MAX_SINCE {
        return Err(format!("{raw}: between 1s and 7d"));
    }
    Ok(d)
}

/// The target from the flags: a request id as forge writes them, or a
/// registered code (else `FG-SYS-0003`).
pub(crate) fn target(request_id: Option<&str>, code: Option<&str>) -> Result<Target, ForgeError> {
    match (request_id, code) {
        (Some(id), None) => RequestId::parse(id.trim())
            .map(|r| Target::RequestId(r.as_str().to_owned()))
            .ok_or_else(|| {
                ForgeError::new(ErrorCode::CliUsageInvalid)
                    .with_context("flag", "--request-id")
                    .with_context("reason", "invalid")
            }),
        (None, Some(code)) => {
            let wanted = code.trim().to_ascii_uppercase();
            ErrorCode::parse(&wanted)
                .map(|c| Target::ErrorCode(c.id().to_owned()))
                .ok_or_else(|| {
                    ForgeError::new(ErrorCode::UnknownErrorCode).with_context("given", wanted)
                })
        }
        // clap requires exactly one.
        (Some(_), Some(_)) | (None, None) => Err(ForgeError::new(ErrorCode::CliUsageInvalid)
            .with_context("flag", "--request-id|--error-code")
            .with_context("reason", "exactly_one")),
    }
}

/// The release's `share/<rel>` next to `bin/forge`, if it exists.
fn shared(rel: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let p = exe.parent()?.parent()?.join("share").join(rel);
    p.exists().then_some(p)
}

/// Read a bounded file (`Err`: the reason).
fn read_bounded(path: &Path, max: u64) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("unreadable: {}", e.kind()))?;
    read_lossy(file, max)
}

/// At most `max` bytes of `from` as text; a byte that is not UTF-8 becomes
/// U+FFFD (one bad byte in a journal window must not lose the window).
fn read_lossy(from: impl std::io::Read, max: u64) -> Result<String, String> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    from.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("unreadable: {}", e.kind()))?;
    if bytes.len() as u64 > max {
        return Err("too_large".to_owned());
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The log stream (`-`: stdin), bounded in size and time. The read runs
/// on a thread of its own, not the runtime's blocking pool: a pipe that
/// never ends must not hold the runtime's shutdown (the thread ends with
/// the process).
pub(crate) async fn read_logs(path: &Path) -> Result<String, Note> {
    let owned = path.to_path_buf();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("forge-diagnose-logs".to_owned())
        .spawn(move || {
            let read = if owned.as_os_str() == "-" {
                read_lossy(std::io::stdin().lock(), MAX_LOG_BYTES)
            } else {
                read_bounded(&owned, MAX_LOG_BYTES)
            };
            // The caller may have given up (timeout): nothing to tell.
            let _gone = tx.send(read);
        });
    if let Err(e) = spawned {
        return Err(Note::new(
            "logs",
            format!("unreadable: {}", e.kind()),
            Some(path),
        ));
    }
    match tokio::time::timeout(LOG_READ_TIMEOUT, rx).await {
        Ok(Ok(Ok(text))) => Ok(text),
        Ok(Ok(Err(reason))) => Err(Note::new("logs", reason, Some(path))),
        Ok(Err(_)) => Err(Note::new("logs", "unreadable: reader_ended", Some(path))),
        Err(_) => Err(Note::new("logs", "timeout", Some(path))),
    }
}

/// The codes to explain: the given one, or those of the request's lines.
fn codes_of(target: &Target, log: &LogSelection) -> Vec<ErrorCode> {
    match target {
        Target::ErrorCode(c) => ErrorCode::parse(c).into_iter().collect(),
        Target::RequestId(_) => log
            .codes
            .iter()
            .filter_map(|c| ErrorCode::parse(c))
            .collect(),
    }
}

/// The recent changes as text (deployments, migrations, configuration).
fn changes_text(
    changes: Option<&forge_db::llm::diagnosis::RecentChanges>,
    config_file: Option<&Path>,
    since: Duration,
) -> String {
    let mut out = format!("window: last {}s\n", since.as_secs());
    match changes {
        Some(c) => {
            out.push_str(&format!("deployments ({}):\n", c.deployments.len()));
            for d in &c.deployments {
                out.push_str(&format!(
                    "- deployment {} project {} {} {} sha {} at {}{}\n",
                    d.id,
                    d.project_id,
                    d.tier,
                    d.status,
                    d.sha,
                    d.at,
                    d.error_code
                        .as_deref()
                        .map(|e| format!(" error_code {e}"))
                        .unwrap_or_default()
                ));
            }
            out.push_str(&format!("migrations ({}):\n", c.migrations.len()));
            for m in &c.migrations {
                out.push_str(&format!(
                    "- {} {} {} at {}\n",
                    m.version,
                    m.description,
                    if m.success { "applied" } else { "FAILED" },
                    m.installed_on
                ));
            }
        }
        None => out.push_str("deployments and migrations: unavailable (see notes)\n"),
    }
    match config_file {
        Some(p) => match std::fs::metadata(p).and_then(|m| m.modified()) {
            Ok(t) => out.push_str(&format!(
                "configuration file modified: {}\n",
                httpdate::fmt_http_date(t)
            )),
            Err(e) => out.push_str(&format!("configuration file: unreadable ({})\n", e.kind())),
        },
        None => out.push_str("configuration file: none (environment only)\n"),
    }
    out.push_str(&format!("forge version: {}\n", env!("CARGO_PKG_VERSION")));
    out
}

/// Everything but the logs and the doctor, from the codes (module docs).
pub(crate) async fn gather(
    db: Option<&Db>,
    args: &Args,
    log: LogSelection,
    doctor: Option<String>,
    config_file: Option<&Path>,
    notes: &mut Vec<Note>,
) -> Facts {
    let codes = codes_of(&args.target, &log);
    let runbooks_dir = args.runbooks.clone().or_else(|| shared("runbooks"));
    let incidents_file = args
        .incidents
        .clone()
        .or_else(|| shared("incidents/records.jsonl"));
    let mut facts = Facts {
        log,
        doctor,
        ..Facts::default()
    };
    if runbooks_dir.is_none() && codes.iter().any(|c| c.entry().runbook.is_some()) {
        notes.push(Note::new("runbooks", "missing", None));
    }
    let incidents = match &incidents_file {
        Some(p) => match read_bounded(p, MAX_FILE_BYTES) {
            Ok(t) => Some(t),
            Err(reason) => {
                notes.push(Note::new("incidents", reason, Some(p)));
                None
            }
        },
        None => {
            notes.push(Note::new("incidents", "missing", None));
            None
        }
    };
    for code in &codes {
        let e = code.entry();
        facts.catalog.push(CatalogFact {
            code: e.id.to_owned(),
            cause: e.cause.to_owned(),
            diagnosis: e.diagnosis.to_owned(),
            fix: e.fix.to_owned(),
            runbook: e.runbook.map(str::to_owned),
        });
        if let (Some(name), Some(dir)) = (e.runbook, &runbooks_dir) {
            // The name comes from the compiled catalog (kebab-case), never
            // from the input.
            let path = dir.join(format!("{name}.md"));
            match read_bounded(&path, MAX_FILE_BYTES) {
                Ok(md) => {
                    let (heading, text) = diagnosis::runbook_section(&md, e.id);
                    if !facts.runbooks.iter().any(|r| r.name == name) {
                        facts.runbooks.push(RunbookFact {
                            name: name.to_owned(),
                            heading,
                            text,
                        });
                    }
                }
                Err(reason) => notes.push(Note::new("runbooks", reason, Some(&path))),
            }
        }
        if let Some(jsonl) = &incidents {
            for rec in diagnosis::incident_records(jsonl, e.id) {
                if !facts.incidents.iter().any(|(id, _)| *id == rec.0) {
                    facts.incidents.push(rec);
                }
            }
        }
    }
    let since = i64::try_from(args.since.as_secs()).unwrap_or(i64::MAX);
    let changes = match db {
        Some(db) => match forge_db::llm::diagnosis::recent_changes(db, since).await {
            Ok(c) => Some(c),
            Err(e) => {
                notes.push(Note::new(
                    "recent_changes",
                    format!("{} {}", e.code(), reason_of(&e)),
                    None,
                ));
                None
            }
        },
        None => None,
    };
    facts.changes = Some(changes_text(changes.as_ref(), config_file, args.since));
    facts
}

fn reason_of(e: &ForgeError) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

/// Prepare every part alone (a refused one is withheld and noted), then
/// all together. `None`: no part could be sent.
pub(crate) fn prepare_facts(facts: &Facts, notes: &mut Vec<Note>) -> Option<PreparedInput> {
    let limits = InputLimits::default();
    let mut kept = Vec::new();
    for part in diagnosis::parts(facts) {
        match prepare(vec![part.clone()], &limits) {
            // clone: the part is prepared alone, then with the others
            Ok(_) => kept.push(part),
            Err(e) => notes.push(Note {
                source: "part",
                reason: format!("{} {} label={}", e.code(), reason_of(&e), part.label),
                path: part.path,
            }),
        }
    }
    if kept.is_empty() {
        return None;
    }
    match prepare(kept, &limits) {
        Ok(input) => Some(input),
        Err(e) => {
            notes.push(Note::new(
                "part",
                format!("{} {}", e.code(), reason_of(&e)),
                None,
            ));
            None
        }
    }
}

/// Ask the gateway (module docs). The gateway logs its own failures; a
/// failed digest check is logged here (the gateway leaves it to callers).
pub(crate) async fn ask(
    gateway: &Gateway,
    catalog: &dyn ModelCatalog,
    input: PreparedInput,
    request_id: &RequestId,
) -> Ai {
    if gateway.is_paused() {
        return Ai::Paused;
    }
    if let Err(e) = gateway.verify_model(catalog).await {
        let e = e.with_context("task", "diagnose_model_check");
        telemetry::log_failure(&e, request_id);
        return Ai::Failed(e);
    }
    let submitted = gateway
        .submit(Submission {
            purpose: Purpose::IncidentDiagnosis,
            priority: None,
            subject_kind: SubjectKind::Incident,
            subject_id: None,
            project: None,
            requested_by: None,
            head_sha: None,
            deep_review: None,
            prompt: forge_llm::prompts::incident_diagnosis(),
            input,
            policy: diagnosis::policy(),
            request_id: request_id.clone(), // clone: the submission owns it
        })
        .await;
    match submitted {
        Ok(Outcome::Advised(a)) => Ai::Advised(a),
        Ok(Outcome::Paused) => Ai::Paused,
        Err(e) => Ai::Failed(e),
    }
}

/// The gateway of this process, or `None` when `llm.enabled` is off.
fn gateway_of(cfg: &Config, db: &Db) -> Result<Option<(Gateway, Arc<OllamaProvider>)>, ForgeError> {
    let llm = &cfg.llm;
    let (true, Some(endpoint), Some(model)) = (llm.enabled, llm.endpoint.as_deref(), &llm.model)
    else {
        return Ok(None);
    };
    let ca = llm.ca_pem()?;
    let provider = Arc::new(OllamaProvider::new(endpoint, ca.as_deref(), llm.timeout)?);
    let gateway = Gateway::new(
        GatewayConfig {
            model: model.clone(), // clone: the gateway owns its settings
            num_ctx: llm.num_ctx,
            concurrency: 1,
            paused: llm.paused,
            call_timeout: llm.timeout,
        },
        provider.clone(), // clone: the provider is also the model catalog
        Arc::new(DbRecords(db.clone())), // clone: the records' own pool handle
    );
    Ok(Some((gateway, provider)))
}

/// `forge diagnose` (module docs): the text to print.
pub(crate) fn run(
    config_file: Option<&Path>,
    args: &Args,
    request_id: &RequestId,
) -> Result<String, ForgeError> {
    let cfg = config::Env::from_process().and_then(|env| config::load(config_file, &env))?;
    block_on(async {
        let mut notes = Vec::new();
        let stream = match &args.logs {
            Some(p) => match read_logs(p).await {
                Ok(t) => Some(t),
                Err(n) => {
                    notes.push(n);
                    None
                }
            },
            None => {
                notes.push(Note::new("logs", "not_given", None));
                None
            }
        };
        let log = stream
            .as_deref()
            .map(|s| diagnosis::select_log_lines(s, &args.target))
            .unwrap_or_default();
        let doctor = if args.doctor {
            match tokio::time::timeout(DOCTOR_TIMEOUT, crate::doctor::json_report(config_file))
                .await
            {
                Ok(text) => Some(text),
                Err(_) => {
                    notes.push(Note::new("doctor", "timeout", None));
                    None
                }
            }
        } else {
            None
        };
        let mut db_error = None;
        let db = match Db::connect(&cfg.database).await {
            Ok(db) => Some(db),
            Err(e) => {
                notes.push(Note::new(
                    "recent_changes",
                    format!("{} {}", e.code(), reason_of(&e)),
                    None,
                ));
                db_error = Some(e.code());
                telemetry::log_failure(&e.with_context("task", "diagnose"), request_id);
                None
            }
        };
        let facts = gather(db.as_ref(), args, log, doctor, config_file, &mut notes).await;
        let input = prepare_facts(&facts, &mut notes);
        let mut model = None;
        let ai = match (&input, args.ask, &db) {
            (_, false, _) => Ai::Skipped,
            (_, true, _) if !cfg.llm.enabled => Ai::Off,
            (None, true, _) => Ai::NoInput,
            (Some(input), true, Some(db)) => match gateway_of(&cfg, db) {
                Ok(Some((gateway, provider))) => {
                    model = Some(gateway.config().model.name.clone());
                    ask(&gateway, &*provider, input.clone(), request_id).await
                    // clone: the bundle is printed after the call
                }
                Ok(None) => Ai::Off,
                Err(e) => {
                    telemetry::log_failure(&e, request_id);
                    Ai::Failed(e)
                }
            },
            // The records need the database; without it nothing is asked
            // (its error was logged above with this request id).
            (Some(_), true, None) => Ai::Failed(
                ForgeError::new(db_error.unwrap_or(ErrorCode::DbUnavailable))
                    .with_context("reason", "diagnose_without_database"),
            ),
        };
        if let Some(db) = &db {
            db.close().await;
        }
        for n in &notes {
            telemetry::log_failure(&n.error(), request_id);
        }
        let report = Report {
            target: args.target.clone(), // clone: the args stay with the caller
            since: args.since,
            request_id: request_id.clone(), // clone: the report owns it
            log: facts.log,
            input,
            notes,
            ai,
            model,
        };
        Ok(if args.json {
            render_json(&report)
        } else {
            render_text(&report)
        })
    })
}

fn class_of(input: Option<&PreparedInput>) -> InputClass {
    input.map_or(InputClass::Public, PreparedInput::class)
}

/// The report for a person (Japanese).
pub(crate) fn render_text(r: &Report) -> String {
    let mut out = format!(
        "障害の一次診断 — {}（直近 {} 秒、問い合わせ番号 {}）\n\n",
        r.target.describe(),
        r.since.as_secs(),
        r.request_id
    );
    match &r.ai {
        Ai::Advised(a) => {
            let advice = &a.advice;
            out.push_str(&format!(
                "■ AI の助言（提案。決めるのは人です。緩和は承認された runbook の手順だけ） model {} / prompt incident_diagnosis v{} / advice #{}{}\n",
                r.model.as_deref().unwrap_or("-"),
                forge_llm::prompts::incident_diagnosis().version,
                a.advice_id,
                if advice.degraded { " / 信頼度が低い" } else { "" }
            ));
            out.push_str(&format!("要約: {}\n", advice.summary));
            let dropped = advice.dropped_mutating
                + advice.dropped_evidence
                + advice.dropped_low_confidence
                + advice.dropped_over_cap;
            if advice.findings.is_empty() {
                out.push_str(if dropped > 0 {
                    "原因の候補: AI は答えたが、候補はすべて検査で捨てた（下の件数）\n"
                } else {
                    "原因の候補: AI は候補を出さなかった\n"
                });
            }
            for (i, f) in advice.findings.iter().enumerate() {
                let at = match (f.path.as_deref(), f.line) {
                    (Some(p), Some(l)) => format!("{p} L{l}"),
                    (Some(p), None) => p.to_owned(),
                    (None, _) => "-".to_owned(),
                };
                out.push_str(&format!(
                    "{}. [{} {:.2}] {}\n   根拠: {} 「{}」\n",
                    i + 1,
                    f.severity.as_str(),
                    f.confidence,
                    f.message,
                    at,
                    f.quote
                ));
            }
            if advice.dropped_mutating > 0 {
                out.push_str(&format!(
                    "（変更の手順を含む候補を {} 件捨てました）\n",
                    advice.dropped_mutating
                ));
            }
            if advice.dropped_evidence > 0 {
                out.push_str(&format!(
                    "（根拠が入力に無い候補を {} 件捨てました）\n",
                    advice.dropped_evidence
                ));
            }
            if advice.dropped_low_confidence > 0 {
                out.push_str(&format!(
                    "（確信度の低い候補を {} 件出していません）\n",
                    advice.dropped_low_confidence
                ));
            }
            if advice.dropped_over_cap > 0 {
                out.push_str(&format!(
                    "（上限 3 件を超えた候補を {} 件出していません）\n",
                    advice.dropped_over_cap
                ));
            }
        }
        Ai::Failed(e) => out.push_str(&format!(
            "■ AI の助言: ありません — {} [{}]（問い合わせ番号 {}）。下の事実の束で診断してください。\n",
            e.user_message(forge_errors::Lang::Ja),
            e.code(),
            r.request_id
        )),
        Ai::Paused => out.push_str(
            "■ AI の助言: ありません — AI のゲートウェイは一時停止中です（llm.paused）。下の事実の束で診断してください。\n",
        ),
        Ai::Off => out.push_str(
            "■ AI の助言: ありません — llm.enabled が off です。下の事実の束で診断してください。\n",
        ),
        Ai::Skipped => out.push_str("■ AI の助言: 求めていません（--facts-only）\n"),
        Ai::NoInput => out.push_str("■ AI の助言: ありません — 渡せる事実がありません\n"),
    }
    out.push_str(&format!(
        "\n■ 事実の束（伏せ字の後。区分 {}。ログ {} 行を読み、該当 {} 行、うち {} 行を載せた）\n",
        class_of(r.input.as_ref()),
        r.log.scanned,
        r.log.matched,
        r.log.lines.len()
    ));
    if r.log.lines.is_empty() {
        out.push_str(
            "（該当するログの行がありません。journalctl -u forge-serve -u forge-worker --since -2h -o cat | forge diagnose ... --logs - で渡します）\n",
        );
    }
    if let Some(input) = &r.input {
        for part in input.parts() {
            out.push_str(&format!(
                "\n--- {} ({}) ---\n{}\n",
                part.path.as_deref().unwrap_or(&part.label),
                part.class,
                part.text
            ));
        }
    }
    if !r.notes.is_empty() {
        out.push_str(&format!(
            "\n■ 読めなかった材料（{}）\n",
            ErrorCode::DiagnoseSourceUnreadable
        ));
        for n in &r.notes {
            out.push_str(&format!(
                "- {}: {}{}\n",
                n.source,
                n.reason,
                n.path
                    .as_deref()
                    .map(|p| format!(" ({p})"))
                    .unwrap_or_default()
            ));
        }
    }
    out
}

/// The report for scripts and the incident record.
pub(crate) fn render_json(r: &Report) -> String {
    let ai = match &r.ai {
        Ai::Advised(a) => json!({
            "status": "advised",
            "advice_id": a.advice_id,
            "llm_request_id": a.llm_request_id,
            "model": r.model,
            "prompt_id": "incident_diagnosis",
            "prompt_version": forge_llm::prompts::incident_diagnosis().version,
            "summary": a.advice.summary,
            "degraded": a.advice.degraded,
            "input_class": a.advice.input_class.as_str(),
            "dropped_mutating": a.advice.dropped_mutating,
            "dropped_evidence": a.advice.dropped_evidence,
            "dropped_low_confidence": a.advice.dropped_low_confidence,
            "dropped_over_cap": a.advice.dropped_over_cap,
            "candidates": forge_llm::findings_json(&a.advice),
        }),
        Ai::Failed(e) => json!({"status": "unavailable", "error_code": e.code().id()}),
        Ai::Paused => json!({"status": "paused"}),
        Ai::Off => json!({"status": "off"}),
        Ai::Skipped => json!({"status": "skipped"}),
        Ai::NoInput => json!({"status": "no_input"}),
    };
    let facts: Vec<_> = r
        .input
        .as_ref()
        .map(|i| {
            i.parts()
                .iter()
                .map(|p| {
                    // The JSON goes into incident records (kept in git):
                    // a confidential part's words stay in the text output.
                    let text = if p.class == InputClass::Confidential {
                        "[confidential: shown in the text output only]"
                    } else {
                        p.text.as_str()
                    };
                    json!({"label": p.label, "path": p.path, "class": p.class.as_str(), "text": text})
                })
                .collect()
        })
        .unwrap_or_default();
    let notes: Vec<_> = r
        .notes
        .iter()
        .map(|n| json!({"source": n.source, "reason": n.reason, "path": n.path}))
        .collect();
    let mut out = json!({
        "target": r.target.describe(),
        "window_s": r.since.as_secs(),
        "request_id": r.request_id.as_str(),
        "advice_label": "AI の助言（提案）",
        "ai": ai,
        "input_class": class_of(r.input.as_ref()).as_str(),
        // false: a source is missing (see notes); the bundle is partial.
        "complete": r.notes.is_empty(),
        "log": {"scanned": r.log.scanned, "matched": r.log.matched, "kept": r.log.lines.len()},
        "facts": facts,
        "notes": notes,
    })
    .to_string();
    out.push('\n');
    out
}

#[cfg(test)]
#[path = "diagnose_tests.rs"]
mod tests;
