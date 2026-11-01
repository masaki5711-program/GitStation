//! `run`: parse, dispatch, render, map to an exit code. All I/O is injected so
//! tests drive the whole program in-process and assert on stdout and stderr.

use crate::cli::{AuthCommand, Cli, Command, GitOperation};
use crate::config::{Config, Overrides};
use crate::credential;
use crate::error::{ClientError, Exit, Result};
use crate::gitcfg;
use crate::hosting;
use crate::http::{ApiClient, Method};
use crate::registry;
use crate::store::{Token, TokenStore};
use clap::Parser;
use forge_errors::ErrorCode;
use serde_json::{json, Map, Value};
use std::io::{Read, Write};

pub struct Io<'a> {
    pub stdin: &'a mut dyn Read,
    pub stdout: &'a mut dyn Write,
    pub stderr: &'a mut dyn Write,
    pub stdin_tty: bool,
    pub stdout_tty: bool,
}

pub type EnvFn<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Run gstn. `args[0]` is the program name. Returns the process exit code.
pub fn run(args: &[String], env: EnvFn<'_>, exe: &str, io: &mut Io<'_>) -> i32 {
    let json_errors = args.iter().any(|a| a == "--json");
    let result = match Cli::try_parse_from(args) {
        Ok(cli) => dispatch(&cli, env, exe, io),
        Err(e) => {
            if matches!(
                e.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                write_out(io, &e.to_string())
            } else {
                Err(ClientError::local(
                    ErrorCode::CliUsageInvalid,
                    format!("reason=args ({})", first_line(&e.to_string())),
                ))
            }
        }
    };
    match result {
        Ok(()) => Exit::Ok.code(),
        Err(e) => {
            let text = if json_errors {
                serde_json::to_string(&e.as_json()).unwrap_or_else(|_| e.human())
            } else {
                e.human()
            };
            // The failure itself is the message; if even stderr is gone there
            // is nowhere left to say so, the exit code still tells.
            if writeln!(io.stderr, "{text}").is_err() {
                return Exit::Other.code();
            }
            e.exit().code()
        }
    }
}

fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim_start_matches("error: ")
        .to_string()
}

fn write_out(io: &mut Io<'_>, text: &str) -> Result<()> {
    io.stdout
        .write_all(text.as_bytes())
        .and_then(|()| io.stdout.flush())
        .map_err(|e| {
            ClientError::local(
                ErrorCode::CliOutputFailed,
                format!("stream=stdout kind={:?}", e.kind()),
            )
        })
}

fn warn(io: &mut Io<'_>, warnings: &[String]) -> Result<()> {
    for w in warnings {
        writeln!(io.stderr, "warning: {w}").map_err(|e| {
            ClientError::local(
                ErrorCode::CliOutputFailed,
                format!("stream=stderr kind={:?}", e.kind()),
            )
        })?;
    }
    Ok(())
}

/// Keep only `fields` at the top level (of each item for a list).
pub fn project(v: Value, fields: &[String]) -> Value {
    if fields.is_empty() {
        return v;
    }
    let pick = |m: Map<String, Value>| -> Value {
        Value::Object(m.into_iter().filter(|(k, _)| fields.contains(k)).collect())
    };
    match v {
        Value::Object(mut m) => {
            let list_key = ["items", "members", "rules"]
                .into_iter()
                .find(|k| matches!(m.get(*k), Some(Value::Array(_))));
            if let Some((key, Value::Array(items))) =
                list_key.and_then(|k| m.remove(k).map(|v| (k, v)))
            {
                let items = items.into_iter().map(|i| project(i, fields)).collect();
                let mut out = Map::new();
                out.insert(key.to_string(), Value::Array(items));
                for k in ["next_cursor", "next_after", "has_more"] {
                    if let Some(x) = m.remove(k) {
                        out.insert(k.to_string(), x);
                    }
                }
                Value::Object(out)
            } else {
                pick(m)
            }
        }
        Value::Array(a) => Value::Array(a.into_iter().map(|i| project(i, fields)).collect()),
        other @ (Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)) => other,
    }
}

pub(crate) fn emit(io: &mut Io<'_>, cli: &Cli, value: Value, human: &str) -> Result<()> {
    if cli.json {
        let v = project(value, &cli.fields);
        let text = if io.stdout_tty {
            serde_json::to_string_pretty(&v)
        } else {
            serde_json::to_string(&v)
        }
        .map_err(|_| ClientError::local(ErrorCode::CliResponseInvalid, "reason=serialize"))?;
        write_out(io, &format!("{text}\n"))
    } else {
        write_out(io, &format!("{human}\n"))
    }
}

pub(crate) fn config_for(cli: &Cli, env: EnvFn<'_>) -> Result<Config> {
    Config::load(
        env,
        &Overrides {
            host: cli.host.clone(),
            config_dir: None,
        },
    )
}

fn dispatch(cli: &Cli, env: EnvFn<'_>, exe: &str, io: &mut Io<'_>) -> Result<()> {
    match &cli.command {
        Command::Help { command } => {
            if cli.json {
                let v = registry::help_json(command);
                emit(io, cli, v, "")
            } else {
                let want = command.join(" ");
                match registry::find(&want).filter(|_| !want.is_empty()) {
                    Some(c) => {
                        let mut s = format!("{}\n  {}\n", c.name, c.summary);
                        for a in c.args {
                            s.push_str(&format!("  {:<14} {}\n", a.name, a.description));
                        }
                        for e in c.examples {
                            s.push_str(&format!("  example: {e}\n"));
                        }
                        write_out(io, &s)
                    }
                    None => write_out(io, &registry::help_text()),
                }
            }
        }
        Command::Schema { command } => {
            let want = command.join(" ");
            let Some(c) = registry::find(&want) else {
                let known: Vec<&str> = registry::COMMANDS.iter().map(|c| c.name).collect();
                return Err(ClientError::local(
                    ErrorCode::CliUsageInvalid,
                    format!("reason=unknown_command (known: {})", known.join(", ")),
                ));
            };
            // Always JSON: the point of `schema` is to be read by a machine.
            let v = project(registry::schema_json(c), &cli.fields);
            write_out(
                io,
                &format!("{}\n", serde_json::to_string(&v).unwrap_or_default()),
            )
        }
        Command::Auth { command } => auth(cli, command, env, exe, io),
        Command::Project { command } => hosting::project(cli, command, env, io),
        Command::Group { command } => hosting::group(cli, command, env, io),
        Command::Member { command } => hosting::member(cli, command, env, io),
        Command::ProtectedRef { command } => hosting::protected_ref(cli, command, env, io),
        Command::Namespace { command } => hosting::namespace(cli, command, env, io),
        Command::User { command } => hosting::user(cli, command, env, io),
        Command::Token { command } => hosting::token(cli, command, env, io),
        Command::Browse { command } => crate::browse::browse(cli, command, env, io),
        Command::Api(a) => api(cli, a, env, io),
    }
}

fn token_store(cfg: &Config, env: EnvFn<'_>) -> Result<TokenStore> {
    TokenStore::open(&cfg.dir, &env("GSTN_CREDENTIAL_STORE").unwrap_or_default())
}

pub(crate) fn require_token(
    cfg: &Config,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<(Token, crate::store::Source)> {
    let store = token_store(cfg, env)?;
    match store.load(&cfg.host_key(), env("GSTN_TOKEN").as_deref())? {
        Some(l) => {
            warn(io, &l.warnings)?;
            Ok((l.token, l.source))
        }
        None => Err(ClientError::local(
            ErrorCode::CliNotLoggedIn,
            format!("host={}", cfg.host_key()),
        )),
    }
}

fn auth(cli: &Cli, cmd: &AuthCommand, env: EnvFn<'_>, exe: &str, io: &mut Io<'_>) -> Result<()> {
    match cmd {
        AuthCommand::Login { with_token } => {
            if !with_token {
                return Err(ClientError::local(
                    ErrorCode::CliUsageInvalid,
                    "reason=with_token_required (gstn never prompts: pipe the token and pass --with-token)",
                ));
            }
            if io.stdin_tty {
                return Err(ClientError::local(
                    ErrorCode::CliUsageInvalid,
                    "reason=stdin_is_a_terminal (pipe the token: gstn auth login --with-token < token.txt)",
                ));
            }
            let mut raw = String::new();
            io.stdin.take(4097).read_to_string(&mut raw).map_err(|_| {
                ClientError::local(ErrorCode::CliUsageInvalid, "reason=stdin_unreadable")
            })?;
            let token = Token::parse(&raw)?;
            let cfg = config_for(cli, env)?;
            let client = ApiClient::new(&cfg, Some(token.clone()))?;
            // Verify first: a wrong token is never stored.
            let me = client.request(Method::Get, "/api/v1/user", &[], None)?.body;
            let store = token_store(&cfg, env)?;
            let saved = store.save(&cfg.host_key(), &token)?;
            warn(io, &saved.warnings)?;
            let username = me
                .get("username")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let out = json!({
                "host": cfg.host_key(),
                "username": username,
                "is_agent": me.get("is_agent").cloned().unwrap_or(Value::Null),
                "token_scopes": me.get("token_scopes").cloned().unwrap_or(Value::Null),
                "stored_in": saved.backend,
            });
            emit(
                io,
                cli,
                out,
                &format!(
                    "Logged in to {} as {username} (token stored in {})",
                    cfg.host_key(),
                    saved.backend
                ),
            )
        }
        AuthCommand::Status => {
            let cfg = config_for(cli, env)?;
            let (token, source) = require_token(&cfg, env, io)?;
            let client = ApiClient::new(&cfg, Some(token))?;
            let me = client.request(Method::Get, "/api/v1/user", &[], None)?.body;
            let get = |k: &str| me.get(k).cloned().unwrap_or(Value::Null);
            let out = json!({
                "host": cfg.host_key(),
                "logged_in": true,
                "user": {
                    "id": get("id"), "username": get("username"), "display_name": get("display_name"),
                    "is_admin": get("is_admin"), "is_agent": get("is_agent"),
                },
                "token_scopes": get("token_scopes"),
                "token_source": source,
            });
            let scopes = get("token_scopes").as_array().map_or_else(
                || "(session)".to_string(),
                |a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                },
            );
            let human = format!(
                "{}\n  user: {} ({})\n  scopes: {scopes}\n  is_agent: {}\n  token from: {}",
                cfg.host_key(),
                get("username").as_str().unwrap_or_default(),
                get("display_name").as_str().unwrap_or_default(),
                get("is_agent"),
                out["token_source"].as_str().unwrap_or_default(),
            );
            emit(io, cli, out, &human)
        }
        AuthCommand::Logout => {
            let cfg = config_for(cli, env)?;
            let d = token_store(&cfg, env)?.delete(&cfg.host_key())?;
            warn(io, &d.warnings)?;
            emit(
                io,
                cli,
                json!({"host": cfg.host_key(), "removed": d.removed}),
                &if d.removed {
                    format!("Logged out of {}", cfg.host_key())
                } else {
                    format!("No stored token for {}", cfg.host_key())
                },
            )
        }
        AuthCommand::GitCredential { install: true, .. } => {
            let cfg = config_for(cli, env)?;
            let done = gitcfg::install(&cfg, exe, env("GIT_CONFIG_GLOBAL").as_deref())?;
            emit(
                io,
                cli,
                json!({"installed": true, "git_config_key": done.key, "helper": done.helper}),
                &format!(
                    "git will use gstn for {} ({} = {})",
                    cfg.host_key(),
                    done.key,
                    done.helper
                ),
            )
        }
        AuthCommand::GitCredential {
            operation: Some(op),
            ..
        } => git_credential(cli, *op, env, io),
        // clap guarantees an operation or --install; reaching here is a bug in
        // the argument definitions, reported as usage rather than a panic.
        AuthCommand::GitCredential {
            operation: None,
            install: false,
        } => Err(ClientError::local(
            ErrorCode::CliUsageInvalid,
            "reason=operation_missing",
        )),
    }
}

fn git_credential(cli: &Cli, op: GitOperation, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    let mut raw = String::new();
    io.stdin
        .take(16 * 1024)
        .read_to_string(&mut raw)
        .map_err(|_| ClientError::local(ErrorCode::CliUsageInvalid, "reason=stdin_unreadable"))?;
    let req = credential::parse_request(&raw);
    let cfg = config_for(cli, env)?;
    // Another host: no answer, no lookup, exit 0.
    if !credential::matches_host(&cfg, &req) {
        return Ok(());
    }
    let store = token_store(&cfg, env)?;
    match op {
        GitOperation::Get => {
            let loaded = store.load(&cfg.host_key(), env("GSTN_TOKEN").as_deref())?;
            if let Some(l) = &loaded {
                warn(io, &l.warnings)?;
            }
            let answer = credential::answer_get(&cfg, &req, loaded.as_ref().map(|l| &l.token));
            write_out(io, &answer)
        }
        // Tokens are issued in the web UI and stored by `auth login`; what git
        // saw typed is not stored (it could be a mistyped password).
        GitOperation::Store => Ok(()),
        GitOperation::Erase => credential::handle_erase(&cfg, &req, &store).map(|_| ()),
    }
}

// ── api ────────────────────────────────────────────────────────────────

pub(crate) fn usage(reason: impl Into<String>) -> ClientError {
    ClientError::local(
        ErrorCode::CliUsageInvalid,
        format!("reason={}", reason.into()),
    )
}

/// `k=v` (string) or `k:=<json>`.
pub fn parse_field(raw: &str) -> Result<(String, Value)> {
    if let Some((k, v)) = raw.split_once(":=") {
        if k.is_empty() {
            return Err(usage("field_key_empty"));
        }
        let val =
            serde_json::from_str(v).map_err(|_| usage(format!("field_json_invalid (key {k})")))?;
        return Ok((k.to_string(), val));
    }
    match raw.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), Value::String(v.to_string()))),
        _ => Err(usage("field_malformed (expected key=value or key:=json)")),
    }
}

fn query_value(k: &str, v: &Value) -> Result<String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Number(_) | Value::Bool(_) => Ok(v.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => {
            Err(usage(format!("query_value_not_scalar (key {k})")))
        }
    }
}

fn api(cli: &Cli, a: &crate::cli::ApiArgs, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    let method = Method::parse(&a.method)
        .ok_or_else(|| usage("method_unknown (GET, POST, PUT, PATCH, DELETE)"))?;
    if a.paginate && method != Method::Get {
        return Err(usage("paginate_needs_get"));
    }
    let fields: Vec<(String, Value)> = a
        .field
        .iter()
        .map(|f| parse_field(f))
        .collect::<Result<_>>()?;
    if a.input.is_some() && !fields.is_empty() {
        return Err(usage("input_and_field_conflict"));
    }
    let as_query = matches!(method, Method::Get | Method::Delete);
    if a.input.is_some() && as_query {
        return Err(usage("input_needs_a_body_method"));
    }
    let mut query = Vec::new();
    let mut body: Option<Value> = None;
    if as_query {
        for (k, v) in &fields {
            query.push((k.clone(), query_value(k, v)?));
        }
    } else if let Some(p) = &a.input {
        let mut text = String::new();
        // Same cap as a response body; over the cap is an error, never a silent cut.
        let limit = crate::http::MAX_BODY_BYTES + 1;
        if p.as_os_str() == "-" {
            io.stdin
                .take(limit)
                .read_to_string(&mut text)
                .map_err(|_| usage("input_unreadable"))?;
        } else {
            std::fs::File::open(p)
                .and_then(|f| f.take(limit).read_to_string(&mut text))
                .map_err(|_| usage(format!("input_unreadable (path {})", p.display())))?;
        }
        if text.len() as u64 > crate::http::MAX_BODY_BYTES {
            return Err(usage("input_too_large (16 MiB at most)"));
        }
        body = Some(serde_json::from_str(&text).map_err(|_| usage("input_not_json"))?);
    } else if !fields.is_empty() {
        body = Some(Value::Object(fields.into_iter().collect()));
    }
    let cfg = config_for(cli, env)?;
    if a.dry_run {
        let client = ApiClient::new(&cfg, None)?;
        let url = client.url_for(&a.path, &query)?;
        let v = json!({"method": method.as_str(), "url": url.as_str(), "body": body, "paginate": a.paginate});
        return write_out(
            io,
            &format!("{}\n", serde_json::to_string(&v).unwrap_or_default()),
        );
    }
    // `api` works without a login for public routes (/healthz); with one, the
    // token is attached. A stored-token read failure is a warning, not a stop.
    let store = token_store(&cfg, env)?;
    let token = match store.load(&cfg.host_key(), env("GSTN_TOKEN").as_deref()) {
        Ok(Some(l)) => {
            warn(io, &l.warnings)?;
            Some(l.token)
        }
        Ok(None) => None,
        Err(e) => {
            warn(
                io,
                &[e.detail()
                    .unwrap_or("credential store unavailable")
                    .to_string()],
            )?;
            None
        }
    };
    let client = ApiClient::new(&cfg, token)?;
    let res = if a.paginate {
        client.get_all_pages(&a.path, &query)?
    } else {
        client.request(method, &a.path, &query, body.as_ref())?
    };
    if res.body.is_null() {
        return Ok(());
    }
    let v = project(res.body, &cli.fields);
    let text = if io.stdout_tty {
        serde_json::to_string_pretty(&v)
    } else {
        serde_json::to_string(&v)
    }
    .map_err(|_| ClientError::local(ErrorCode::CliResponseInvalid, "reason=serialize"))?;
    write_out(io, &format!("{text}\n"))
}
