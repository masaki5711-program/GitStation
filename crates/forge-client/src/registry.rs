//! The command registry: the one list `help --json`, `schema <command>`, the
//! human `help`, and the C-1 route-coverage gate are generated from (plan 01
//! §3.11 C-5). A test keeps it equal to the clap command tree, so a command
//! cannot exist without being described, nor be described without existing.

use serde_json::{json, Value};

pub struct ArgSpec {
    pub name: &'static str,
    /// `positional`, `option` (takes a value) or `flag`.
    pub kind: &'static str,
    pub required: bool,
    pub repeatable: bool,
    pub description: &'static str,
}

pub struct CommandSpec {
    /// Space-separated path, e.g. `auth login`.
    pub name: &'static str,
    pub summary: &'static str,
    pub args: &'static [ArgSpec],
    pub examples: &'static [&'static str],
    /// Token scopes the API calls need (`read_api`, `api`, …); empty: none.
    pub scopes: &'static [&'static str],
    /// The API v1 routes it calls: `METHOD /path`.
    pub routes: &'static [&'static str],
    /// JSON Schema (as text) of the `--json` standard output.
    pub output: &'static str,
}

const fn arg(
    name: &'static str,
    kind: &'static str,
    required: bool,
    description: &'static str,
) -> ArgSpec {
    ArgSpec {
        name,
        kind,
        required,
        repeatable: false,
        description,
    }
}

pub const GLOBAL_ARGS: &[ArgSpec] = &[
    arg("--json", "flag", false, "Machine-readable output: JSON on stdout, and on failure a JSON error {error_code,user_message,request_id,retryable,detail} on stderr."),
    arg("--host", "option", false, "Server base URL (https://…); overrides GSTN_HOST and config.toml."),
    ArgSpec { name: "--fields", kind: "option", required: false, repeatable: false, description: "Comma-separated top-level fields to keep in --json output (applied to each item of a list)." },
];

pub const EXIT_CODES: &[(i32, &str)] = &[
    (0, "success"),
    (1, "other failure"),
    (2, "usage or configuration error"),
    (3, "not authenticated, or not allowed"),
    (4, "not found"),
    (5, "conflict"),
    (6, "temporary failure; retrying may help"),
];

pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "auth login",
        summary: "Verify a token against the server and store it (OS credential store, else a 0600 file).",
        args: &[arg("--with-token", "flag", true, "Read the token from standard input (never prompts, never an argument).")],
        examples: &["gstn auth login --with-token < token.txt", "echo $TOKEN | gstn --host https://git.example.internal auth login --with-token"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/user"],
        output: r#"{"type":"object","required":["host","username","is_agent","token_scopes","stored_in"],"properties":{"host":{"type":"string"},"username":{"type":"string"},"is_agent":{"type":"boolean"},"token_scopes":{"type":["array","null"],"items":{"type":"string"}},"stored_in":{"enum":["keyring","file"]}}}"#,
    },
    CommandSpec {
        name: "auth status",
        summary: "Show who the stored token belongs to: user, scopes, is_agent. Never the token.",
        args: &[],
        examples: &["gstn auth status --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/user"],
        output: r#"{"type":"object","required":["host","logged_in","user","token_scopes","token_source"],"properties":{"host":{"type":"string"},"logged_in":{"const":true},"user":{"type":"object","required":["id","username","display_name","is_admin","is_agent"]},"token_scopes":{"type":["array","null"],"items":{"type":"string"}},"token_source":{"enum":["env","keyring","file"]}}}"#,
    },
    CommandSpec {
        name: "auth logout",
        summary: "Remove the stored token for the host (does not revoke it on the server).",
        args: &[],
        examples: &["gstn auth logout"],
        scopes: &[],
        routes: &[],
        output: r#"{"type":"object","required":["host","removed"],"properties":{"host":{"type":"string"},"removed":{"type":"boolean"}}}"#,
    },
    CommandSpec {
        name: "auth git-credential",
        summary: "git credential helper for the configured host only (get/store/erase), or --install it in the global git config.",
        args: &[
            arg("operation", "positional", false, "get | store | erase (git passes it; reads key=value lines on stdin)."),
            arg("--install", "flag", false, "Register this helper for the configured host: git config --global credential.<host>.helper."),
        ],
        examples: &["gstn auth git-credential --install", "git clone https://git.example.internal/team/app.git"],
        scopes: &["read_repository", "write_repository"],
        routes: &[],
        output: r#"{"type":"object","description":"--install only; get/store/erase speak the git credential protocol on stdout","required":["installed","git_config_key","helper"],"properties":{"installed":{"type":"boolean"},"git_config_key":{"type":"string"},"helper":{"type":"string"}}}"#,
    },
    CommandSpec {
        name: "api",
        summary: "Call any REST API v1 route: gstn api <METHOD> <path>. GET/DELETE fields become the query, other methods send a JSON body.",
        args: &[
            arg("method", "positional", true, "GET | POST | PUT | PATCH | DELETE"),
            arg("path", "positional", true, "A path on the configured host, e.g. /api/v1/projects"),
            ArgSpec { name: "--field", kind: "option", required: false, repeatable: true, description: "key=value (a string) or key:=<json> (a JSON value)." },
            arg("--input", "option", false, "Send this file (or - for stdin) as the JSON body."),
            arg("--paginate", "flag", false, "GET only: follow next_cursor and merge all items into one list."),
            arg("--dry-run", "flag", false, "Print the request that would be sent (no token) and send nothing."),
        ],
        examples: &["gstn api GET /api/v1/projects --paginate", "gstn api POST /api/v1/projects --field name=app --field 'parent_id:=12'", "gstn api PUT /api/v1/projects/7/visibility --input body.json"],
        scopes: &["read_api", "api"],
        routes: &[],
        output: r#"{"description":"the response body of API v1, as sent (a list when --paginate)"}"#,
    },
    CommandSpec {
        name: "help",
        summary: "Describe the commands; with --json, every command, argument, example, required scope and route.",
        args: &[ArgSpec { name: "command", kind: "positional", required: false, repeatable: true, description: "Limit to one command, e.g. auth login." }],
        examples: &["gstn help --json"],
        scopes: &[],
        routes: &[],
        output: r#"{"type":"object","required":["gstn_version","api_version","global_args","exit_codes","commands"]}"#,
    },
    CommandSpec {
        name: "schema",
        summary: "The machine-readable description of one command: arguments, output JSON Schema, exit codes.",
        args: &[ArgSpec { name: "command", kind: "positional", required: true, repeatable: true, description: "The command path, e.g. auth login." }],
        examples: &["gstn schema auth status"],
        scopes: &[],
        routes: &[],
        output: r#"{"type":"object","required":["command","summary","args","output","exit_codes"]}"#,
    },
];

pub fn find(path: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|c| c.name == path)
}

fn arg_json(a: &ArgSpec) -> Value {
    json!({
        "name": a.name,
        "kind": a.kind,
        "required": a.required,
        "repeatable": a.repeatable,
        "description": a.description,
    })
}

fn exit_json() -> Value {
    Value::Array(
        EXIT_CODES
            .iter()
            .map(|(c, d)| json!({"code": c, "meaning": d}))
            .collect(),
    )
}

pub fn command_json(c: &CommandSpec) -> Value {
    let output: Value = serde_json::from_str(c.output).unwrap_or(Value::Null);
    json!({
        "name": c.name,
        "summary": c.summary,
        "args": c.args.iter().map(arg_json).collect::<Vec<_>>(),
        "examples": c.examples,
        "scopes": c.scopes,
        "routes": c.routes,
        "output": output,
    })
}

/// `help --json`.
pub fn help_json(filter: &[String]) -> Value {
    let want = filter.join(" ");
    let commands: Vec<Value> = COMMANDS
        .iter()
        .filter(|c| want.is_empty() || c.name == want)
        .map(command_json)
        .collect();
    json!({
        "gstn_version": env!("CARGO_PKG_VERSION"),
        "api_version": "v1",
        "global_args": GLOBAL_ARGS.iter().map(arg_json).collect::<Vec<_>>(),
        "exit_codes": exit_json(),
        "commands": commands,
    })
}

/// `schema <command>`.
pub fn schema_json(c: &CommandSpec) -> Value {
    let mut v = command_json(c);
    if let Value::Object(m) = &mut v {
        m.insert("exit_codes".to_string(), exit_json());
        m.insert(
            "error".to_string(),
            json!({"stderr_with_json": ["error_code", "user_message", "request_id", "retryable", "detail"]}),
        );
    }
    v
}

pub fn help_text() -> String {
    let mut s = String::from("gstn - GitStation client (REST API v1)\n\nCommands:\n");
    for c in COMMANDS {
        s.push_str(&format!("  {:<20} {}\n", c.name, c.summary));
    }
    s.push_str("\nGlobal options:\n");
    for a in GLOBAL_ARGS {
        s.push_str(&format!("  {:<10} {}\n", a.name, a.description));
    }
    s.push_str("\nExit codes:\n");
    for (c, d) in EXIT_CODES {
        s.push_str(&format!("  {c}  {d}\n"));
    }
    s.push_str("\nMachine-readable: gstn help --json, gstn schema <command>\n");
    s
}
