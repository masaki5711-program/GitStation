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

const PROJECT_FIELDS: &str = r#"{"type":"object","required":["id","namespace_id","path","name","display_name","description","visibility","created_at","role"],"properties":{"id":{"type":"integer"},"namespace_id":{"type":"integer"},"path":{"type":"string"},"name":{"type":"string"},"display_name":{"type":"string"},"description":{"type":"string"},"visibility":{"enum":["private","internal"]},"created_at":{"type":"string"},"role":{"type":["string","null"]}}}"#;
const PROJECT_SCHEMA: &str = PROJECT_FIELDS;
const PROJECT_PAGE_SCHEMA: &str = r#"{"type":"object","required":["items","next_cursor","has_more"],"properties":{"items":{"type":"array","items":{"type":"object","required":["id","namespace_id","path","name","display_name","description","visibility","created_at","role"]}},"next_cursor":{"type":["string","null"]},"has_more":{"type":"boolean"}}}"#;
const GROUP_SCHEMA: &str = r#"{"type":"object","required":["id","namespace_id","parent_id","path","name","display_name","description","created_at","role"],"properties":{"id":{"type":"integer"},"namespace_id":{"type":"integer"},"parent_id":{"type":["integer","null"]},"path":{"type":"string"},"name":{"type":"string"},"display_name":{"type":"string"},"description":{"type":"string"},"created_at":{"type":"string"},"role":{"type":["string","null"]}}}"#;
const MEMBER_SCHEMA: &str = r#"{"type":"object","required":["user_id","username","display_name","role","active","created_at"],"properties":{"user_id":{"type":"integer"},"username":{"type":"string"},"display_name":{"type":"string"},"role":{"type":"string"},"active":{"type":"boolean"},"created_at":{"type":"string"}}}"#;
const MOVE_SCHEMA: &str = r#"{"type":"object","required":["id","kind","namespace_id","old_path","path","tombstones","projects","reclaimed_tombstones"],"properties":{"id":{"type":"integer"},"kind":{"enum":["project","group","user"]},"namespace_id":{"type":"integer"},"old_path":{"type":"string"},"path":{"type":"string"},"tombstones":{"type":"integer"},"projects":{"type":"integer"},"reclaimed_tombstones":{"type":"integer"}}}"#;
const PROTECTED_REF_SCHEMA: &str = r#"{"type":"object","required":["id","pattern","push_min_role","created_at","updated_at"],"properties":{"id":{"type":"integer"},"pattern":{"type":"string"},"push_min_role":{"type":"string"},"created_at":{"type":"string"},"updated_at":{"type":"string"}}}"#;

const MR_SCHEMA: &str = r#"{"type":"object","required":["project_id","iid","title","description","state","draft","source_branch","target_branch","author_id","head_sha","created_at","updated_at","closed_at","closed_by"],"properties":{"project_id":{"type":"integer"},"iid":{"type":"integer"},"title":{"type":"string"},"description":{"type":"string"},"state":{"enum":["open","merged","closed"]},"draft":{"type":"boolean"},"source_branch":{"type":"string"},"target_branch":{"type":"string"},"author_id":{"type":"integer"},"head_sha":{"type":["string","null"]},"created_at":{"type":"string"},"updated_at":{"type":"string"},"closed_at":{"type":["string","null"]},"closed_by":{"type":["integer","null"]}}}"#;

const COMMIT_DETAIL_SCHEMA: &str = r#"{"type":"object","required":["commit","message","diff"],"properties":{"commit":{"type":"object","required":["oid","parents","author_name","author_email","authored_at","committer_name","committer_email","committed_at","subject"]},"message":{"type":"string"},"diff":{"type":"object","required":["files","truncated"],"properties":{"files":{"type":"array","items":{"type":"object","required":["status","old_path","new_path","old_mode","new_mode","old_oid","new_oid","similarity","binary","patch","omitted"],"properties":{"status":{"enum":["added","modified","deleted","renamed","copied","type_changed"]},"patch":{"type":["string","null"]},"omitted":{"enum":["too_large","total_cap","not_utf8",null]}}}},"truncated":{"type":"boolean"}}}}}"#;

const SA_SCHEMA: &str = r#"{"type":"object","required":["id","username","display_name","state","owner","purpose","created_by","created_at","active_tokens","last_token_expiry"],"properties":{"id":{"type":"integer"},"username":{"type":"string"},"display_name":{"type":"string"},"state":{"enum":["active","disabled"]},"owner":{"type":["object","null"],"required":["id","username","active"]},"purpose":{"type":["string","null"]},"created_by":{"type":["integer","null"]},"created_at":{"type":"string"},"active_tokens":{"type":"integer"},"last_token_expiry":{"type":["string","null"]}}}"#;

const CHECK_RUN_SCHEMA: &str = r#"{"type":"object","required":["id","name","head_sha","status","conclusion","summary","details_url","annotations","reported_by","revision","created_at","updated_at","completed_at"],"properties":{"id":{"type":"integer"},"name":{"type":"string"},"head_sha":{"type":"string"},"status":{"enum":["queued","in_progress","completed"]},"conclusion":{"enum":["success","neutral","failure","error","action_required",null]},"summary":{"type":"string"},"details_url":{"type":["string","null"]},"annotations":{"type":"array","items":{"type":"object","required":["path","start_line","end_line","level","message","title"]}},"reported_by":{"type":["object","null"],"required":["id","username"]},"revision":{"type":"integer"},"created_at":{"type":"string"},"updated_at":{"type":"string"},"completed_at":{"type":["string","null"]}}}"#;
const BINDINGS_SCHEMA: &str = r#"{"type":"object","required":["items"],"properties":{"items":{"type":"array","items":{"type":"object","required":["integration_id","kind","project_id","external_project_id","granted_membership","created_by","created_at"]}}}}"#;

const REVISION_PATH_SCHEMA: &str = r#"{"type":"object","required":["ref","path"],"properties":{"ref":{"type":"object","required":["type","name","full_name","commit"],"properties":{"type":{"enum":["branch","tag","commit"]},"name":{"type":"string"},"full_name":{"type":["string","null"]},"commit":{"type":"string"}}},"path":{"type":"string"}}}"#;

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
        name: "project list",
        summary: "List the projects you can see (one page; --all follows next_cursor, bounded).",
        args: &[
            arg("--limit", "option", false, "Rows per page, 1..=100 (server default 20; 100 with --all)."),
            arg("--cursor", "option", false, "Continue after this next_cursor (not with --all)."),
            arg("--all", "flag", false, "Follow next_cursor and merge every page (bounded)."),
        ],
        examples: &["gstn project list --json", "gstn project list --all --fields id,path"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects"],
        output: PROJECT_PAGE_SCHEMA,
    },
    CommandSpec {
        name: "project view",
        summary: "Show one project by id.",
        args: &[arg("id", "positional", true, "The project id (a positive whole number; `namespace resolve` finds it from a path).")],
        examples: &["gstn project view 7 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}"],
        output: PROJECT_SCHEMA,
    },
    CommandSpec {
        name: "project create",
        summary: "Create a project in a namespace (a user's or a group's).",
        args: &[
            arg("--namespace-id", "option", true, "The parent namespace id."),
            arg("--path", "option", true, "The path segment of the new project."),
            arg("--display-name", "option", true, "The display name."),
            arg("--description", "option", false, "A description."),
            arg("--visibility", "option", false, "private | internal (server default private)."),
        ],
        examples: &["gstn project create --namespace-id 3 --path app --display-name 'Team app'"],
        scopes: &["api"],
        routes: &["POST /api/v1/projects"],
        output: PROJECT_SCHEMA,
    },
    CommandSpec {
        name: "project edit",
        summary: "Change a project's display name and/or description (at least one).",
        args: &[
            arg("id", "positional", true, "The project id."),
            arg("--display-name", "option", false, "The new display name."),
            arg("--description", "option", false, "The new description (may be empty)."),
        ],
        examples: &["gstn project edit 7 --display-name 'New name'"],
        scopes: &["api"],
        routes: &["PATCH /api/v1/projects/{id}"],
        output: PROJECT_SCHEMA,
    },
    CommandSpec {
        name: "project visibility",
        summary: "Set a project's visibility (private | internal). Needs --yes.",
        args: &[
            arg("id", "positional", true, "The project id."),
            arg("--visibility", "option", true, "private | internal"),
            arg("--yes", "flag", true, "Confirm (gstn never prompts)."),
        ],
        examples: &["gstn project visibility 7 --visibility internal --yes"],
        scopes: &["api"],
        routes: &["PUT /api/v1/projects/{id}/visibility"],
        output: PROJECT_SCHEMA,
    },
    CommandSpec {
        name: "project move",
        summary: "Move a project to another namespace and/or rename it (--path alone). Its old path stops working for others. Needs --yes.",
        args: &[
            arg("id", "positional", true, "The project id."),
            arg("--namespace-id", "option", false, "The new parent namespace id (at least one of --namespace-id, --path)."),
            arg("--path", "option", false, "The new last path segment."),
            arg("--reclaim", "flag", false, "Take back an old path of this very project (A -> B -> A)."),
            arg("--yes", "flag", true, "Confirm (gstn never prompts)."),
        ],
        examples: &["gstn project move 7 --namespace-id 4 --yes", "gstn project move 7 --path app2 --yes"],
        scopes: &["api"],
        routes: &["POST /api/v1/projects/{id}/transfer"],
        output: MOVE_SCHEMA,
    },
    CommandSpec {
        name: "project delete",
        summary: "Delete a project; it stays restorable for the grace period. Needs --yes.",
        args: &[
            arg("id", "positional", true, "The project id."),
            arg("--yes", "flag", true, "Confirm (gstn never prompts)."),
        ],
        examples: &["gstn project delete 7 --yes"],
        scopes: &["api"],
        routes: &["DELETE /api/v1/projects/{id}"],
        output: r#"{"type":"object","required":["id","deleted_at","purge_after"],"properties":{"id":{"type":"integer"},"deleted_at":{"type":"string"},"purge_after":{"type":"string"}}}"#,
    },
    CommandSpec {
        name: "project restore",
        summary: "Restore a deleted project within its grace period.",
        args: &[arg("id", "positional", true, "The project id.")],
        examples: &["gstn project restore 7"],
        scopes: &["api"],
        routes: &["POST /api/v1/projects/{id}/restore"],
        output: PROJECT_SCHEMA,
    },
    CommandSpec {
        name: "group list",
        summary: "List the groups you can see (one page; --all follows next_cursor, bounded).",
        args: &[
            arg("--limit", "option", false, "Rows per page, 1..=100 (server default 20; 100 with --all)."),
            arg("--cursor", "option", false, "Continue after this next_cursor (not with --all)."),
            arg("--all", "flag", false, "Follow next_cursor and merge every page (bounded)."),
        ],
        examples: &["gstn group list --json", "gstn group list --limit 50 --cursor <next_cursor>"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/groups"],
        output: r#"{"type":"object","required":["items","next_cursor","has_more"],"properties":{"items":{"type":"array","items":{"type":"object","required":["id","namespace_id","path","name","display_name","description","created_at","role"]}},"next_cursor":{"type":["string","null"]},"has_more":{"type":"boolean"}}}"#,
    },
    CommandSpec {
        name: "group view",
        summary: "Show one group by id.",
        args: &[arg("id", "positional", true, "The group id.")],
        examples: &["gstn group view 3 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/groups/{id}"],
        output: GROUP_SCHEMA,
    },
    CommandSpec {
        name: "group create",
        summary: "Create a top-level group, or a subgroup of --parent-id.",
        args: &[
            arg("--path", "option", true, "The path segment."),
            arg("--display-name", "option", true, "The display name."),
            arg("--description", "option", false, "A description."),
            arg("--parent-id", "option", false, "Create it inside this group (default: top level)."),
        ],
        examples: &["gstn group create --path team --display-name Team", "gstn group create --parent-id 3 --path web --display-name Web"],
        scopes: &["api"],
        routes: &["POST /api/v1/groups", "POST /api/v1/groups/{id}/subgroups"],
        output: GROUP_SCHEMA,
    },
    CommandSpec {
        name: "group edit",
        summary: "Change a group's display name and/or description (at least one).",
        args: &[
            arg("id", "positional", true, "The group id."),
            arg("--display-name", "option", false, "The new display name."),
            arg("--description", "option", false, "The new description (may be empty)."),
        ],
        examples: &["gstn group edit 3 --description 'Platform team'"],
        scopes: &["api"],
        routes: &["PATCH /api/v1/groups/{id}"],
        output: GROUP_SCHEMA,
    },
    CommandSpec {
        name: "group move",
        summary: "Move a group and its subtree and/or rename it. Old paths stop working for others. Needs --yes.",
        args: &[
            arg("id", "positional", true, "The group id."),
            arg("--namespace-id", "option", false, "The new parent group's namespace id (alias --namespace; not with --top-level)."),
            arg("--top-level", "flag", false, "Move to the top level (administrators only)."),
            arg("--path", "option", false, "The new last path segment (at least one of --namespace-id, --top-level, --path)."),
            arg("--reclaim", "flag", false, "Take back an old path of this group's subtree."),
            arg("--yes", "flag", true, "Confirm (gstn never prompts)."),
        ],
        examples: &["gstn group move 3 --namespace-id 9 --yes", "gstn group move 3 --top-level --yes", "gstn group move 3 --path platform --yes"],
        scopes: &["api"],
        routes: &["POST /api/v1/groups/{id}/transfer"],
        output: MOVE_SCHEMA,
    },
    CommandSpec {
        name: "member list",
        summary: "List the direct members of a project or a group (keyset by user id; --all follows next_after, bounded).",
        args: &[
            arg("--project", "option", false, "The project id (exactly one of --project, --group)."),
            arg("--group", "option", false, "The group id (exactly one of --project, --group)."),
            arg("--limit", "option", false, "Members per page, 1..=100 (server default 20; 100 with --all)."),
            arg("--after", "option", false, "Continue after this user id (a previous next_after; not with --all)."),
            arg("--all", "flag", false, "Follow next_after and merge every page (bounded)."),
        ],
        examples: &["gstn member list --project 7 --all --json", "gstn member list --group 3 --limit 20 --after 41"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/members", "GET /api/v1/groups/{id}/members"],
        output: r#"{"type":"object","required":["members","next_after"],"properties":{"members":{"type":"array","items":{"type":"object","required":["user_id","username","display_name","role","active","created_at"]}},"next_after":{"type":["integer","null"]}}}"#,
    },
    CommandSpec {
        name: "member add",
        summary: "Make a user a direct member of a project or a group.",
        args: &[
            arg("--project", "option", false, "The project id (exactly one of --project, --group)."),
            arg("--group", "option", false, "The group id (exactly one of --project, --group)."),
            arg("--user-id", "option", true, "The user id."),
            arg("--role", "option", true, "guest | reporter | developer | maintainer | owner"),
        ],
        examples: &["gstn member add --project 7 --user-id 12 --role developer"],
        scopes: &["api"],
        routes: &["POST /api/v1/projects/{id}/members", "POST /api/v1/groups/{id}/members"],
        output: MEMBER_SCHEMA,
    },
    CommandSpec {
        name: "member set-role",
        summary: "Change a member's direct role.",
        args: &[
            arg("--project", "option", false, "The project id (exactly one of --project, --group)."),
            arg("--group", "option", false, "The group id (exactly one of --project, --group)."),
            arg("--user-id", "option", true, "The user id."),
            arg("--role", "option", true, "guest | reporter | developer | maintainer | owner"),
        ],
        examples: &["gstn member set-role --group 3 --user-id 12 --role maintainer"],
        scopes: &["api"],
        routes: &["PATCH /api/v1/projects/{id}/members/{user_id}", "PATCH /api/v1/groups/{id}/members/{user_id}"],
        output: MEMBER_SCHEMA,
    },
    CommandSpec {
        name: "member remove",
        summary: "Remove a direct member. Needs --yes.",
        args: &[
            arg("--project", "option", false, "The project id (exactly one of --project, --group)."),
            arg("--group", "option", false, "The group id (exactly one of --project, --group)."),
            arg("--user-id", "option", true, "The user id."),
            arg("--yes", "flag", true, "Confirm (gstn never prompts)."),
        ],
        examples: &["gstn member remove --project 7 --user-id 12 --yes"],
        scopes: &["api"],
        routes: &["DELETE /api/v1/projects/{id}/members/{user_id}", "DELETE /api/v1/groups/{id}/members/{user_id}"],
        output: r#"{"type":"object","required":["removed","target","target_id","user_id"],"properties":{"removed":{"const":true},"target":{"enum":["project","group"]},"target_id":{"type":"integer"},"user_id":{"type":"integer"}}}"#,
    },
    CommandSpec {
        name: "protected-ref list",
        summary: "List a project's protected ref rules.",
        args: &[arg("--project", "option", true, "The project id.")],
        examples: &["gstn protected-ref list --project 7 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/protected-refs"],
        output: r#"{"type":"object","required":["rules"],"properties":{"rules":{"type":"array","items":{"type":"object","required":["id","pattern","push_min_role","created_at","updated_at"]}}}}"#,
    },
    CommandSpec {
        name: "protected-ref add",
        summary: "Protect a ref pattern: only roles from --push-min-role up may push to matching refs.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--pattern", "option", true, "refs/heads/… or refs/tags/…, at most one * in the last part."),
            arg("--push-min-role", "option", true, "developer | maintainer | owner"),
        ],
        examples: &["gstn protected-ref add --project 7 --pattern 'refs/heads/main' --push-min-role maintainer"],
        scopes: &["api"],
        routes: &["POST /api/v1/projects/{id}/protected-refs"],
        output: PROTECTED_REF_SCHEMA,
    },
    CommandSpec {
        name: "protected-ref edit",
        summary: "Change a rule's minimum push role (the pattern is fixed: remove the rule and add another).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--rule-id", "option", true, "The rule id (from protected-ref list)."),
            arg("--push-min-role", "option", true, "developer | maintainer | owner"),
        ],
        examples: &["gstn protected-ref edit --project 7 --rule-id 2 --push-min-role owner"],
        scopes: &["api"],
        routes: &["PATCH /api/v1/projects/{id}/protected-refs/{rule_id}"],
        output: PROTECTED_REF_SCHEMA,
    },
    CommandSpec {
        name: "protected-ref remove",
        summary: "Remove a protected ref rule. Needs --yes.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--rule-id", "option", true, "The rule id (from protected-ref list)."),
            arg("--yes", "flag", true, "Confirm (gstn never prompts)."),
        ],
        examples: &["gstn protected-ref remove --project 7 --rule-id 2 --yes"],
        scopes: &["api"],
        routes: &["DELETE /api/v1/projects/{id}/protected-refs/{rule_id}"],
        output: r#"{"type":"object","required":["removed","project_id","rule_id"],"properties":{"removed":{"const":true},"project_id":{"type":"integer"},"rule_id":{"type":"integer"}}}"#,
    },
    CommandSpec {
        name: "approval-rule list",
        summary: "List a project's approval rules (name, required approvals, target pattern, approvers with their names, updated_at).",
        args: &[arg("--project", "option", true, "The project id.")],
        examples: &["gstn approval-rule list --project 7 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/approval-rules"],
        output: r#"{"type":"object","required":["rules"],"properties":{"rules":{"type":"array","items":{"type":"object","required":["approval_rule_id","name","target_pattern","required_approvals","user_ids","group_ids","users","groups","updated_at"]}}}}"#,
    },
    CommandSpec {
        name: "approval-rule set",
        summary: "Create or replace an approval rule (people only; Maintainer): how many counted approvals a merge request into a matching target needs, and from whom (no --user and no --group: any eligible Developer). Existing approvals are kept; the merge decision changes at once. It reads the rules first and sends the rule's updated_at (null for a new name): a rule changed, removed or added under the name in between is refused (FG-REVIEW-0018; run it again), never overwritten.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--name", "option", true, "The rule's name (a-z 0-9 _ . -)."),
            arg("--required", "option", true, "Counted approvals needed (1..=20)."),
            arg("--target-pattern", "option", false, "A protected-ref pattern such as refs/heads/main; absent: every target."),
            arg("--user", "option", false, "A username who may approve (repeatable)."),
            arg("--group", "option", false, "A group full path whose direct members may approve (repeatable)."),
        ],
        examples: &["gstn approval-rule set --project 7 --name backend --required 2 --target-pattern refs/heads/main --group acme/backend"],
        scopes: &["api"],
        routes: &["GET /api/v1/projects/{id}/approval-rules", "PUT /api/v1/projects/{id}/approval-rules/{name}"],
        output: r#"{"type":"object","required":["approval_rule_id","name","target_pattern","required_approvals","user_ids","group_ids","users","groups","updated_at"]}"#,
    },
    CommandSpec {
        name: "approval-rule remove",
        summary: "Remove an approval rule (people only; Maintainer). --yes confirms.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--name", "option", true, "The rule's name."),
            arg("--yes", "flag", true, "Confirm the removal."),
        ],
        examples: &["gstn approval-rule remove --project 7 --name backend --yes"],
        scopes: &["api"],
        routes: &["DELETE /api/v1/projects/{id}/approval-rules/{name}"],
        output: r#"{"type":"object","required":["removed","project_id","name"]}"#,
    },
    CommandSpec {
        name: "merge-settings show",
        summary: "Show a project's merge settings: the methods allowed and the default, whether the source branch is deleted by default, whether new commits clear approvals, whether committers' approvals are excluded, and CODEOWNERS enforcement (suspended until when).",
        args: &[arg("--project", "option", true, "The project id.")],
        examples: &["gstn merge-settings show --project 7 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/merge-settings"],
        output: r#"{"type":"object","required":["project_id","allowed_methods","default_method","delete_source_default","reset_approvals_on_push","committer_approvals_excluded","codeowners","stored","updated_at"]}"#,
    },
    CommandSpec {
        name: "merge-settings set",
        summary: "Change a project's merge settings (people only; Maintainer). Options left out keep their value; --allow replaces the allowed methods. It sends the updated_at it read: settings changed in between are refused (FG-REVIEW-0017; run it again), never overwritten.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--allow", "option", false, "An allowed method (repeatable): merge_commit, squash, fast_forward."),
            arg("--default", "option", false, "The default method (one of the allowed)."),
            arg("--delete-source-branch", "option", false, "true / false: delete the source branch after a merge by default."),
            arg("--reset-approvals-on-push", "option", false, "true / false: new commits end the approvals (false: carry them when the change allows it)."),
            arg("--exclude-committer-approvals", "option", false, "true / false: committers' approvals do not count."),
        ],
        examples: &["gstn merge-settings set --project 7 --allow merge_commit --allow squash --default squash"],
        scopes: &["api"],
        routes: &["GET /api/v1/projects/{id}/merge-settings", "PUT /api/v1/projects/{id}/merge-settings"],
        output: r#"{"type":"object","required":["project_id","allowed_methods","default_method","delete_source_default","reset_approvals_on_push","committer_approvals_excluded","codeowners","stored","updated_at"]}"#,
    },
    CommandSpec {
        name: "codeowners-enforcement suspend",
        summary: "Suspend CODEOWNERS enforcement for 1..=1440 minutes (people only; Maintainer at or above every protection of the project). It comes back by itself at the expiry. An approved runbook step (runbook codeowners).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--for", "option", true, "How long, in minutes (1..=1440)."),
        ],
        examples: &["gstn codeowners-enforcement suspend --project 7 --for 60"],
        scopes: &["api"],
        routes: &["PUT /api/v1/projects/{id}/merge-settings/codeowners-enforcement"],
        output: r#"{"type":"object","required":["project_id","codeowners"],"properties":{"codeowners":{"type":"object","required":["enforced","suspended_at","suspended_until"]}}}"#,
    },
    CommandSpec {
        name: "codeowners-enforcement enforce",
        summary: "Enforce CODEOWNERS again now (people only; Maintainer).",
        args: &[arg("--project", "option", true, "The project id.")],
        examples: &["gstn codeowners-enforcement enforce --project 7"],
        scopes: &["api"],
        routes: &["PUT /api/v1/projects/{id}/merge-settings/codeowners-enforcement"],
        output: r#"{"type":"object","required":["project_id","codeowners"],"properties":{"codeowners":{"type":"object","required":["enforced","suspended_at","suspended_until"]}}}"#,
    },
    CommandSpec {
        name: "merge-rule set",
        summary: "Set a protected branch rule's merge conditions (people only; Maintainer; never above one's own role): minimum approvals, required checks (ci/, kanflow/, forge/ only; no run reported blocks), resolved discussions, the direct-push ban (merges still land), and (M3 I1d) a work item link (warns `work_item_link_unbound` on a project without an integration binding). A required check no active service account may report blocks every merge (the answer warns `required_checks_unreportable` and names them in `unreportable_checks`; grant the namespace with admin service-account grant-checks). Reads the merge rule first: options left out keep their value, and it sends the merge rule's updated_at (null when there is none): a merge rule changed, removed or added in between is refused (FG-REVIEW-0019; run it again), never overwritten.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--rule-id", "option", true, "The protection rule's id (from protected-ref list)."),
            arg("--approvals", "option", false, "Minimum counted approvals (0..=20)."),
            arg("--check", "option", false, "A required check name (repeatable; replaces the list)."),
            arg("--clear-checks", "flag", false, "Require no check."),
            arg("--resolved-discussions", "option", false, "true / false: every discussion must be resolved."),
            arg("--direct-push-forbidden", "option", false, "true / false: forbid direct pushes to the branch (FG-GIT-0017)."),
            arg("--work-item-link", "option", false, "true / false: require an open link to a work item (PBI-<n>) of the project's integration binding (work_item_link_missing)."),
        ],
        examples: &["gstn merge-rule set --project 7 --rule-id 2 --approvals 1 --resolved-discussions true --direct-push-forbidden true", "gstn merge-rule set --project 7 --rule-id 2 --work-item-link true"],
        scopes: &["api"],
        routes: &["GET /api/v1/projects/{id}/protected-refs", "PUT /api/v1/projects/{id}/protected-refs/{rule_id}/merge-rule"],
        output: r#"{"type":"object","required":["id","pattern","push_min_role","merge_rule","warnings"],"properties":{"merge_rule":{"type":"object","required":["required_approvals","required_checks","require_resolved_discussions","direct_push_forbidden","require_work_item_link","updated_at"]}}}"#,
    },
    CommandSpec {
        name: "merge-rule remove",
        summary: "Remove a protected branch rule's merge conditions (people only; Maintainer). --yes confirms.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--rule-id", "option", true, "The protection rule's id."),
            arg("--yes", "flag", true, "Confirm the removal."),
        ],
        examples: &["gstn merge-rule remove --project 7 --rule-id 2 --yes"],
        scopes: &["api"],
        routes: &["DELETE /api/v1/projects/{id}/protected-refs/{rule_id}/merge-rule"],
        output: r#"{"type":"object","required":["removed","project_id","rule_id"]}"#,
    },
    CommandSpec {
        name: "mr create",
        summary: "Open a merge request from one existing branch into another (Developer).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--source", "option", true, "The source branch (feature/x or refs/heads/feature/x)."),
            arg("--target", "option", true, "The target branch (another branch than the source)."),
            arg("--title", "option", true, "1 to 255 characters."),
            arg("--description", "option", false, "Free text (may be empty)."),
            arg("--draft", "flag", false, "Open it as a draft."),
        ],
        examples: &["gstn mr create --project 7 --source feature/login --target main --title 'Add the login form' --json"],
        scopes: &["api"],
        routes: &["POST /api/v1/projects/{id}/merge-requests"],
        output: MR_SCHEMA,
    },
    CommandSpec {
        name: "mr list",
        summary: "List a project's merge requests, newest first (open by default).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--state", "option", false, "open | closed | merged | all (default open)."),
            arg("--limit", "option", false, "Rows per page, 1..=100."),
            arg("--before", "option", false, "Continue below this next_before iid."),
            arg("--all", "flag", false, "Follow next_before and merge every page (bounded)."),
        ],
        examples: &["gstn mr list --project 7 --json", "gstn mr list --project 7 --state all --all"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/merge-requests"],
        output: r#"{"type":"object","required":["merge_requests","next_before"],"properties":{"merge_requests":{"type":"array","items":{"type":"object","required":["project_id","iid","title","state","draft","source_branch","target_branch","author_id"]}},"next_before":{"type":["integer","null"]}}}"#,
    },
    CommandSpec {
        name: "mr view",
        summary: "Show one merge request.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--iid", "option", true, "The merge request's number in the project."),
        ],
        examples: &["gstn mr view --project 7 --iid 3 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/merge-requests/{iid}"],
        output: MR_SCHEMA,
    },
    CommandSpec {
        name: "mr edit",
        summary: "Change a merge request's title, description, target or draft flag (the author or Developer; another's draft flag: Maintainer).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--iid", "option", true, "The merge request's number in the project."),
            arg("--title", "option", false, "A new title."),
            arg("--description", "option", false, "A new description (may be empty)."),
            arg("--target", "option", false, "A new target branch (only while open)."),
            arg("--draft", "flag", false, "Mark it a draft (only while open)."),
            arg("--ready", "flag", false, "Mark it ready (only while open)."),
        ],
        examples: &["gstn mr edit --project 7 --iid 3 --ready", "gstn mr edit --project 7 --iid 3 --title 'Add the sign-in form'"],
        scopes: &["api"],
        routes: &["PATCH /api/v1/projects/{id}/merge-requests/{iid}"],
        output: MR_SCHEMA,
    },
    CommandSpec {
        name: "mr close",
        summary: "Close a merge request (the author or Developer).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--iid", "option", true, "The merge request's number in the project."),
        ],
        examples: &["gstn mr close --project 7 --iid 3"],
        scopes: &["api"],
        routes: &["POST /api/v1/projects/{id}/merge-requests/{iid}/close"],
        output: MR_SCHEMA,
    },
    CommandSpec {
        name: "mr reopen",
        summary: "Reopen a closed merge request; its branches must still exist (never a merged one).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--iid", "option", true, "The merge request's number in the project."),
        ],
        examples: &["gstn mr reopen --project 7 --iid 3"],
        scopes: &["api"],
        routes: &["POST /api/v1/projects/{id}/merge-requests/{iid}/reopen"],
        output: MR_SCHEMA,
    },
    CommandSpec {
        name: "mr versions",
        summary: "List a merge request's versions (one per pushed head), oldest first, with their bases.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--iid", "option", true, "The merge request's number in the project."),
        ],
        examples: &["gstn mr versions --project 7 --iid 3 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/merge-requests/{iid}/versions"],
        output: r#"{"type":"object","required":["iid","versions"],"properties":{"iid":{"type":"integer"},"versions":{"type":"array","items":{"type":"object","required":["version","head_sha","base_sha","merge_base_sha","pushed_by","created_at","refs_purged"]}}}}"#,
    },
    CommandSpec {
        name: "mr diff",
        summary: "One version's diff (merge base to head), or two versions compared head to head with --from / --to (capped like browse compare; a purged version is not found).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--iid", "option", true, "The merge request's number in the project."),
            arg("--version", "option", false, "The version whose diff to show (or --from / --to)."),
            arg("--from", "option", false, "The earlier version of a comparison (with --to)."),
            arg("--to", "option", false, "The later version of a comparison (with --from)."),
            arg("--patch", "flag", false, "Print the patches too (--json always has them)."),
        ],
        examples: &[
            "gstn mr diff --project 7 --iid 3 --version 2 --patch",
            "gstn mr diff --project 7 --iid 3 --from 1 --to 2 --json",
        ],
        scopes: &["read_api"],
        routes: &[
            "GET /api/v1/projects/{id}/merge-requests/{iid}/versions/{version}/diff",
            "GET /api/v1/projects/{id}/merge-requests/{iid}/compare",
        ],
        output: r#"{"type":"object","required":["commits","commits_truncated","diff"],"properties":{"version":{"type":"object","required":["version","head_sha"]},"from":{"type":"string"},"to":{"type":"string"},"from_version":{"type":"integer"},"to_version":{"type":"integer"},"from_sha":{"type":"string"},"to_sha":{"type":"string"},"commits":{"type":"array","items":{"type":"object","required":["oid","parents","subject"]}},"commits_truncated":{"type":"boolean"},"diff":{"type":"object","required":["files","truncated"]}}}"#,
    },
    CommandSpec {
        name: "mr mergeability",
        summary: "Whether a merge request can be merged, as last checked (unchecked / checking / can_be_merged / cannot_be_merged / check_failed, the reason and the conflicted files); never starts a check.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--iid", "option", true, "The merge request's number in the project."),
        ],
        examples: &["gstn mr mergeability --project 7 --iid 3 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/merge-requests/{iid}/mergeability"],
        output: r#"{"type":"object","required":["iid","status","reason","stale","head_sha","checked_head_sha","checked_target_sha","checked_at","conflicts","conflicts_truncated"],"properties":{"status":{"type":"string","enum":["unchecked","checking","can_be_merged","cannot_be_merged","check_failed"]},"conflicts":{"type":"array","items":{"type":"string"}}}}"#,
    },
    CommandSpec {
        name: "mr merge",
        summary: "Merge a merge request: queue the merge of the head you reviewed (--sha; another head is refused). People only; the target branch's push role; the token needs merge_execute (api does not merge). --wait waits until it is applied, or exits with the refusal's code (conflict, a moved target, …).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--iid", "option", true, "The merge request's number in the project."),
            arg("--sha", "option", true, "The head commit you reviewed (mr view shows head_sha)."),
            arg("--method", "option", false, "merge_commit (default), squash, or fast_forward when the project allows it."),
            arg("--delete-source-branch", "flag", false, "Delete the source branch after the merge (decided as your branch delete; a protected branch is kept)."),
            arg("--message", "option", false, "The commit message (default: from the title and description)."),
            arg("--wait", "flag", false, "Wait until the merge is applied, superseded or failed."),
            arg("--wait-timeout", "option", false, "How long --wait waits, in seconds (default 300; FG-CLI-0008 when it passes)."),
        ],
        examples: &[
            "gstn mr merge --project 7 --iid 3 --sha 0123456789abcdef0123456789abcdef01234567 --wait",
            "gstn mr merge --project 7 --iid 3 --sha 0123456789abcdef0123456789abcdef01234567 --method squash --delete-source-branch --json",
        ],
        scopes: &["merge_execute", "read_api"],
        routes: &[
            "POST /api/v1/projects/{id}/merge-requests/{iid}/merge",
            "GET /api/v1/projects/{id}/merge-requests/{iid}/merge",
        ],
        output: r#"{"type":"object","required":["merge_id","iid","state","method","head_sha","target_branch","delete_source_branch","expected_old","new_sha","reason","error_code","source_outcome","request_id","created_at","updated_at","finished_at"],"properties":{"state":{"type":"string","enum":["queued","prepared","executing","applied","superseded","failed"]},"method":{"type":"string","enum":["merge_commit","squash","fast_forward"]}}}"#,
    },
    CommandSpec {
        name: "mr approve",
        summary: "Approve the head you reviewed (--sha; another head is refused). People only; Developer; never your own merge request; the token needs merge_approve (api does not approve). The answer says whether the approval counts now (counted / uncounted_reason: committer, committers_unknown, not_in_rule, …); a new commit or a new target clears it.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--iid", "option", true, "The merge request's number in the project."),
            arg("--sha", "option", true, "The head commit you reviewed (mr view shows head_sha)."),
        ],
        examples: &["gstn mr approve --project 7 --iid 3 --sha 0123456789abcdef0123456789abcdef01234567"],
        scopes: &["merge_approve"],
        routes: &["POST /api/v1/projects/{id}/merge-requests/{iid}/approve"],
        output: r#"{"type":"object","required":["iid","outcome","counted","uncounted_reason","approval"],"properties":{"outcome":{"type":"string","enum":["approved","withdrawn","unchanged"]}}}"#,
    },
    CommandSpec {
        name: "mr unapprove",
        summary: "Withdraw your approval of a merge request (unchanged when you have none).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--iid", "option", true, "The merge request's number in the project."),
        ],
        examples: &["gstn mr unapprove --project 7 --iid 3"],
        scopes: &["merge_approve"],
        routes: &["POST /api/v1/projects/{id}/merge-requests/{iid}/unapprove"],
        output: r#"{"type":"object","required":["iid","outcome","counted","uncounted_reason","approval"],"properties":{"outcome":{"type":"string","enum":["approved","withdrawn","unchanged"]}}}"#,
    },
    CommandSpec {
        name: "mr approvals",
        summary: "Evaluate a merge request's approvals now: how each approval counts (and why not), the approval rules, the CODEOWNERS rules the changes need, and blocked_by (approvals_missing, codeowner_approval_missing, codeowners_invalid, config_change_needs_maintainer_approval, changed_paths_unknown).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--iid", "option", true, "The merge request's number in the project."),
        ],
        examples: &["gstn mr approvals --project 7 --iid 3 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/merge-requests/{iid}/approvals"],
        output: r#"{"type":"object","required":["iid","head_sha","target_branch","approvals","rules","codeowners","config_change_approved","blocked_by","conflict"],"properties":{"blocked_by":{"type":"array","items":{"type":"object","required":["reason","detail"]}}}}"#,
    },
    CommandSpec {
        name: "mr work-items",
        summary: "A merge request's work item links (kanflow PBI-<n> of the project's binding), open and ended, oldest first: detected in the title or the source branch (source auto) or linked by a person (manual). An ended link stays (unlinked_at, unlink_source).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--iid", "option", true, "The merge request's number in the project."),
        ],
        examples: &["gstn mr work-items --project 7 --iid 3 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/merge-requests/{iid}/work-items"],
        output: r#"{"type":"object","required":["iid","links","truncated"],"properties":{"links":{"type":"array","items":{"type":"object","required":["link_id","work_item_id","work_item","source","linked_by","linked_at","unlinked_at","unlink_source","unlinked_by"]}}}}"#,
    },
    CommandSpec {
        name: "mr link-work-item",
        summary: "Link a work item (--ref PBI-<n> of the project's kanflow binding) to a merge request by hand. Developer and above; FG-INT-0018 when the project is not bound. unchanged when it is linked already (by detection or a person).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--iid", "option", true, "The merge request's number in the project."),
            arg("--ref", "option", true, "The work item, PBI-<n>."),
        ],
        examples: &["gstn mr link-work-item --project 7 --iid 3 --ref PBI-43"],
        scopes: &["api"],
        routes: &["PUT /api/v1/projects/{id}/merge-requests/{iid}/work-items/{ref}"],
        output: r#"{"type":"object","required":["iid","outcome","link_id","work_item_id","work_item","source"],"properties":{"outcome":{"type":"string","enum":["linked","unchanged"]}}}"#,
    },
    CommandSpec {
        name: "mr unlink-work-item",
        summary: "Unlink a work item (--ref PBI-<n>) from a merge request. Developer and above. The link stays in the history and detection does not link it again (link it by hand to bring it back). Not linked now: FG-REPO-0005.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--iid", "option", true, "The merge request's number in the project."),
            arg("--ref", "option", true, "The work item, PBI-<n> or its work_item_id (a link to an earlier binding's work item)."),
        ],
        examples: &["gstn mr unlink-work-item --project 7 --iid 3 --ref PBI-43"],
        scopes: &["api"],
        routes: &["DELETE /api/v1/projects/{id}/merge-requests/{iid}/work-items/{ref}"],
        output: r#"{"type":"object","required":["unlinked","project_id","iid","ref"]}"#,
    },
    CommandSpec {
        name: "mr status",
        summary: "Whether a merge request may merge now and every reason it may not, in ADR-0024's fixed order (the first is the one-line reason), with the approvals, the required checks, the discussions and CODEOWNERS enforcement. With --json: the stable merge status contract (schema_version 1, docs/api/merge-status.md) for agents.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--iid", "option", true, "The merge request's number in the project."),
        ],
        examples: &["gstn mr status --project 7 --iid 3 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/merge-requests/{iid}/status"],
        output: r#"{"type":"object","required":["schema_version","iid","head_sha","target_branch","method","mergeable","reasons","approvals","checks","discussions","codeowners"],"properties":{"schema_version":{"const":1},"mergeable":{"type":"boolean"},"reasons":{"type":"array","items":{"type":"object","required":["reason","detail"]}},"checks":{"type":"array","items":{"type":"object","required":["name","status","conclusion"]}}}}"#,
    },
    CommandSpec {
        name: "mr merge-status",
        summary: "The latest merge attempt of a merge request (queued / prepared / executing / applied / superseded / failed, its reason and code); --wait waits until it is final.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--iid", "option", true, "The merge request's number in the project."),
            arg("--wait", "flag", false, "Wait until the latest attempt is final."),
            arg("--wait-timeout", "option", false, "How long --wait waits, in seconds (default 300)."),
        ],
        examples: &["gstn mr merge-status --project 7 --iid 3 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/merge-requests/{iid}/merge"],
        output: r#"{"type":"object","required":["iid","merge"],"properties":{"merge":{"type":["object","null"]}}}"#,
    },
    CommandSpec {
        name: "mr discussions",
        summary: "List a merge request's discussions with their notes, oldest first; an inline thread is `outdated` when a later version rewrote its line.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--iid", "option", true, "The merge request's number in the project."),
        ],
        examples: &["gstn mr discussions --project 7 --iid 3 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/merge-requests/{iid}/discussions"],
        output: r#"{"type":"object","required":["iid","discussions","truncated"],"properties":{"discussions":{"type":"array","items":{"type":"object","required":["id","kind","position","outdated","started_by","resolved","resolved_by","resolved_at","created_at","notes"]}},"truncated":{"type":"boolean"}}}"#,
    },
    CommandSpec {
        name: "mr comment",
        summary: "Start a discussion — general, or inline on a line of a version's diff (--version, --path, --line, --side) — or reply to one (--discussion). Agents' tokens may comment.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--iid", "option", true, "The merge request's number in the project."),
            arg("--body", "option", true, "The note: plain text / Markdown, stored as written (at most 60 KiB)."),
            arg("--discussion", "option", false, "Reply to this discussion."),
            arg("--version", "option", false, "An inline discussion: the version whose diff the line is in."),
            arg("--path", "option", false, "An inline discussion: the file's path on that side."),
            arg("--line", "option", false, "An inline discussion: a line inside a hunk of that file."),
            arg("--side", "option", false, "An inline discussion: old (merge base) or new (head, the default)."),
        ],
        examples: &[
            "gstn mr comment --project 7 --iid 3 --body 'Looks good'",
            "gstn mr comment --project 7 --iid 3 --version 2 --path src/lib.rs --line 10 --body 'Rename this'",
            "gstn mr comment --project 7 --iid 3 --discussion 12 --body 'Done' --json",
        ],
        scopes: &["api"],
        routes: &[
            "POST /api/v1/projects/{id}/merge-requests/{iid}/discussions",
            "POST /api/v1/projects/{id}/merge-requests/{iid}/discussions/{discussion_id}/notes",
        ],
        output: r#"{"type":"object","required":["id","kind","position","outdated","started_by","resolved","notes"]}"#,
    },
    CommandSpec {
        name: "mr resolve",
        summary: "Resolve a discussion, or open it again with --unresolve (people only: its starter, or a Developer who is not the merge request's author; agents' and service accounts' tokens are refused).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--iid", "option", true, "The merge request's number in the project."),
            arg("--discussion", "option", true, "The discussion's id."),
            arg("--unresolve", "flag", false, "Open a resolved discussion again."),
        ],
        examples: &[
            "gstn mr resolve --project 7 --iid 3 --discussion 12",
            "gstn mr resolve --project 7 --iid 3 --discussion 12 --unresolve --json",
        ],
        scopes: &["api"],
        routes: &[
            "POST /api/v1/projects/{id}/merge-requests/{iid}/discussions/{discussion_id}/resolve",
            "POST /api/v1/projects/{id}/merge-requests/{iid}/discussions/{discussion_id}/unresolve",
        ],
        output: r#"{"type":"object","required":["id","resolved","resolved_by","resolved_at","notes"]}"#,
    },
    CommandSpec {
        name: "conflicts",
        summary: "A merge request's predicted conflicts with the other open merge requests on its target branch, drafts included (M3 X6): the last scan (scanned / incomplete with why / failed) and each merge request whose head conflicts with this one in a file both change, with the files. The server scans after every new head, retarget and reopen; this never starts a scan.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--iid", "option", true, "The merge request's number in the project."),
        ],
        examples: &[
            "gstn conflicts --project 7 --iid 3",
            "gstn conflicts --project 7 --iid 3 --json",
        ],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/merge-requests/{iid}/conflicts"],
        output: r#"{"type":"object","required":["iid","head_sha","scan","predictions"],"properties":{"scan":{"type":"object","required":["status","reason","stale","scanned_head_sha","scanned_target_sha","scanned_at","peers","pairs_tried","pairs_skipped"],"properties":{"status":{"type":"string","enum":["none","pending","scanning","scanned","incomplete","failed"]}}},"predictions":{"type":"array","items":{"type":"object","required":["iid","draft","head_sha","outcome","reason","conflicts","conflicts_truncated","checked_at"],"properties":{"outcome":{"type":"string","enum":["conflict","failed"]}}}}}}"#,
    },
    CommandSpec {
        name: "namespace resolve",
        summary: "Which project, group or user a full path names (and its ids).",
        args: &[arg("path", "positional", true, "The full path, e.g. team/app.")],
        examples: &["gstn namespace resolve team/app --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/namespaces/resolve"],
        output: r#"{"type":"object","required":["namespace_id","kind","canonical_path","moved"],"properties":{"namespace_id":{"type":"integer"},"kind":{"enum":["project","group","user"]},"canonical_path":{"type":"string"},"moved":{"type":"boolean"},"project_id":{"type":"integer"},"group_id":{"type":"integer"},"user_id":{"type":"integer"}}}"#,
    },
    CommandSpec {
        name: "namespace release-path",
        summary: "Release a moved or deleted path's tombstone so the path can be used again. Needs --yes.",
        args: &[
            arg("path", "positional", true, "The tombstoned full path, exactly (group/sub/project)."),
            arg("--yes", "flag", true, "Confirm (gstn never prompts)."),
        ],
        examples: &["gstn namespace release-path team/old-app --yes"],
        scopes: &["api"],
        routes: &["POST /api/v1/namespaces/tombstones/release"],
        output: r#"{"type":"object","required":["path","kind"],"properties":{"path":{"type":"string"},"kind":{"enum":["moved","deleted"]}}}"#,
    },
    CommandSpec {
        name: "browse refs",
        summary: "List a project's branches and tags (with the commit each peels to) and the default branch.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--type", "option", false, "branch | tag (default: both)."),
        ],
        examples: &["gstn browse refs --project 7 --json", "gstn browse refs --project 7 --type tag"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/repository/refs"],
        output: r#"{"type":"object","required":["default_branch","refs","truncated"],"properties":{"default_branch":{"type":["string","null"]},"refs":{"type":"array","items":{"type":"object","required":["name","full_name","type","commit","target"],"properties":{"name":{"type":"string"},"full_name":{"type":"string"},"type":{"enum":["branch","tag"]},"commit":{"type":["string","null"]},"target":{"type":"string"}}}},"truncated":{"type":"boolean"}}}"#,
    },
    CommandSpec {
        name: "browse resolve",
        summary: "Split <ref>/<path> into a revision and a path (full commit id first, then the longest branch, then the longest tag).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("spec", "positional", true, "The part of a page URL after /-/tree/ or /-/blob/, e.g. feature/x/src/lib.rs."),
        ],
        examples: &["gstn browse resolve --project 7 feature/x/src/lib.rs --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/repository/resolve"],
        output: REVISION_PATH_SCHEMA,
    },
    CommandSpec {
        name: "browse tree",
        summary: "List one directory of a revision (capped by the server; truncated past the cap).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--ref", "option", true, "A full commit id, a branch or a tag (refs/heads/... / refs/tags/... or a short name)."),
            arg("--path", "option", false, "The directory (default: the root)."),
        ],
        examples: &["gstn browse tree --project 7 --ref main --path src --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/repository/tree"],
        output: r#"{"type":"object","required":["ref","path","oid","entries","truncated"],"properties":{"ref":{"type":"object","required":["type","name","full_name","commit"]},"path":{"type":"string"},"oid":{"type":"string"},"entries":{"type":"array","items":{"type":"object","required":["name","path","type","mode","oid","size"],"properties":{"name":{"type":"string"},"path":{"type":"string"},"type":{"enum":["tree","blob","symlink","submodule"]},"mode":{"type":"string"},"oid":{"type":"string"},"size":{"type":["integer","null"]}}}},"truncated":{"type":"boolean"}}}"#,
    },
    CommandSpec {
        name: "browse file",
        summary: "Show one file of a revision: its text (at most 1 MiB; truncated past it), or that it is binary.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--ref", "option", true, "A full commit id, a branch or a tag."),
            arg("--path", "option", true, "The file's path."),
        ],
        examples: &["gstn browse file --project 7 --ref main --path README.md", "gstn browse file --project 7 --ref v1.0 --path src/lib.rs --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/repository/blob"],
        output: r#"{"type":"object","required":["ref","path","name","type","mode","oid","size","binary","content","truncated"],"properties":{"ref":{"type":"object","required":["type","name","full_name","commit"]},"path":{"type":"string"},"name":{"type":"string"},"type":{"enum":["blob","symlink"]},"mode":{"type":"string"},"oid":{"type":"string"},"size":{"type":"integer"},"binary":{"type":"boolean"},"content":{"type":["string","null"]},"truncated":{"type":"boolean"}}}"#,
    },
    CommandSpec {
        name: "browse commits",
        summary: "List a revision's history, newest first, one page at a time (optionally only the commits that changed --path).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--ref", "option", true, "A full commit id, a branch or a tag."),
            arg("--path", "option", false, "Only commits that changed this path."),
            arg("--limit", "option", false, "Commits per page, 1..=100 (server default 30)."),
            arg("--cursor", "option", false, "Continue after this next_cursor (use the same --path)."),
        ],
        examples: &["gstn browse commits --project 7 --ref main --json", "gstn browse commits --project 7 --ref main --path src/lib.rs --limit 50 --cursor <next_cursor>"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/repository/commits"],
        output: r#"{"type":"object","required":["ref","head","path","commits","next_cursor","has_more"],"properties":{"ref":{"type":"object","required":["type","name","full_name","commit"]},"head":{"type":"string"},"path":{"type":"string"},"commits":{"type":"array","items":{"type":"object","required":["oid","parents","author_name","author_email","authored_at","committer_name","committer_email","committed_at","subject"]}},"next_cursor":{"type":["string","null"]},"has_more":{"type":"boolean"}}}"#,
    },
    CommandSpec {
        name: "browse commit",
        summary: "Show one commit: message and the diff against its first parent (per-file patches under server caps; --patch prints them).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("oid", "positional", true, "The full commit id (40 or 64 hexadecimal digits)."),
            arg("--patch", "flag", false, "Print the patches too (--json always has them)."),
        ],
        examples: &["gstn browse commit --project 7 0123456789abcdef0123456789abcdef01234567 --patch", "gstn browse commit --project 7 0123456789abcdef0123456789abcdef01234567 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/repository/commits/{oid}"],
        output: COMMIT_DETAIL_SCHEMA,
    },
    CommandSpec {
        name: "browse compare",
        summary: "Compare two revisions: the commits --to has and --from does not, and the tree diff from --from to --to (capped; truncated past the caps).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--from", "option", true, "The base: a full commit id, a branch or a tag."),
            arg("--to", "option", true, "The head, named like --from."),
            arg("--patch", "flag", false, "Print the patches too (--json always has them)."),
        ],
        examples: &["gstn browse compare --project 7 --from v1.0 --to main --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/repository/compare"],
        output: r#"{"type":"object","required":["from","to","commits","commits_truncated","diff"],"properties":{"from":{"type":"object","required":["type","name","full_name","commit"]},"to":{"type":"object","required":["type","name","full_name","commit"]},"commits":{"type":"array","items":{"type":"object","required":["oid","parents","subject"]}},"commits_truncated":{"type":"boolean"},"diff":{"type":"object","required":["files","truncated"]}}}"#,
    },
    CommandSpec {
        name: "browse blame",
        summary: "Show who last changed each line of a file (hunks; at most 10000 lines; binary files have none).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--ref", "option", true, "A full commit id, a branch or a tag."),
            arg("--path", "option", true, "The file's path."),
        ],
        examples: &["gstn browse blame --project 7 --ref main --path src/lib.rs", "gstn browse blame --project 7 --ref main --path src/lib.rs --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/repository/blame"],
        output: r#"{"type":"object","required":["ref","path","oid","size","binary","hunks","commits","truncated"],"properties":{"ref":{"type":"object","required":["type","name","full_name","commit"]},"path":{"type":"string"},"oid":{"type":"string"},"size":{"type":"integer"},"binary":{"type":"boolean"},"hunks":{"type":"array","items":{"type":"object","required":["commit","start","orig_start","lines"]}},"commits":{"type":"array","items":{"type":"object","required":["oid","author_name","author_email","author_time","author_tz","summary","boundary","path"]}},"truncated":{"type":"boolean"}}}"#,
    },
    CommandSpec {
        name: "browse spec-refs",
        summary: "Show a revision's .forge/repo.yml and its spec references, each resolved with your permissions (unreadable ones are unavailable).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--ref", "option", false, "A full commit id, a branch or a tag (default: the default branch)."),
        ],
        examples: &["gstn browse spec-refs --project 7", "gstn browse spec-refs --project 7 --ref main --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/repository/spec-refs"],
        output: r#"{"type":"object","required":["ref","path","config","problem","specs"],"properties":{"ref":{"type":"object","required":["type","name","full_name","commit"]},"path":{"type":"string"},"config":{"enum":["absent","valid","invalid"]},"problem":{"type":["object","null"],"required":["error_code","line","reason"]},"specs":{"type":"array","items":{"type":"object","required":["line","type","repo","path","pin","scope","status","error_code","project_id","commit"],"properties":{"type":{"enum":["repo","path"]},"status":{"enum":["resolved","version_not_found","unavailable","local"]}}}}}}"#,
    },
    CommandSpec {
        name: "spec requirements",
        summary: "List a revision's own specification: its requirements (searchable), documents and document problems (M3 X4).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--ref", "option", false, "A full commit id, a branch or a tag (default: the default branch)."),
            arg("--search", "option", false, "Only requirements whose id or text contains this (case-insensitive)."),
            arg("--limit", "option", false, "At most this many requirements, 1..=500 (server default 100)."),
        ],
        examples: &["gstn spec requirements --project 7", "gstn spec requirements --project 7 --ref v1.4 --search reload --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/specs/requirements"],
        output: r#"{"type":"object","required":["ref","config","dirs","documents","requirements","truncated","problems","incomplete","withheld"],"properties":{"ref":{"type":"object","required":["type","name","full_name","commit"]},"config":{"enum":["absent","valid","invalid"]},"dirs":{"type":"array","items":{"type":"string"}},"documents":{"type":"array","items":{"type":"object","required":["path","spec","title","status","requirements","problems","confidential"]}},"requirements":{"type":"array","items":{"type":"object","required":["id","path","line","priority","deprecated","spec","body"]}},"truncated":{"type":"boolean"},"problems":{"type":"array","items":{"type":"object","required":["error_code","path","line","rule","detail"]}},"incomplete":{"type":["object","null"],"required":["error_code","reason"]}}}"#,
    },
    CommandSpec {
        name: "spec versions",
        summary: "List the specification's versions (version tags), newest first (M3 X4).",
        args: &[arg("--project", "option", true, "The project id.")],
        examples: &["gstn spec versions --project 7"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/specs/versions"],
        output: r#"{"type":"object","required":["versions","truncated"],"properties":{"versions":{"type":"array","items":{"type":"object","required":["name","full_name","commit"]}},"truncated":{"type":"boolean"}}}"#,
    },
    CommandSpec {
        name: "spec diff",
        summary: "Show the requirements added, changed, removed or deprecated between two revisions (M3 X4).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--from", "option", true, "The earlier revision: a commit id, a branch or a tag."),
            arg("--to", "option", true, "The later revision: a commit id, a branch or a tag."),
        ],
        examples: &["gstn spec diff --project 7 --from v1.4 --to v1.5"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/specs/diff"],
        output: r#"{"type":"object","required":["from","to","changes","incomplete","withheld"],"properties":{"from":{"type":"object","required":["type","name","full_name","commit"]},"to":{"type":"object","required":["type","name","full_name","commit"]},"changes":{"type":"array","items":{"type":"object","required":["id","kind","path","line"],"properties":{"kind":{"enum":["added","changed","removed","deprecated"]}}}},"incomplete":{"type":["object","null"],"required":["error_code","reason"]}}}"#,
    },
    CommandSpec {
        name: "spec trace",
        summary: "Show each requirement (own, and every pinned reference you may read) with the @req tests naming it (M3 X4).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--ref", "option", false, "A full commit id, a branch or a tag (default: the default branch)."),
        ],
        examples: &["gstn spec trace --project 7", "gstn spec trace --project 7 --ref main --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/specs/trace"],
        output: r#"{"type":"object","required":["ref","requirements","references","incomplete","withheld"],"properties":{"ref":{"type":"object","required":["type","name","full_name","commit"]},"requirements":{"type":"array","items":{"type":"object","required":["id","source","path","line","deprecated","status","test_count","tests"],"properties":{"status":{"enum":["tested","untested"]}}}},"references":{"type":"array","items":{"type":"object","required":["line","repo","status","error_code","requirements"],"properties":{"status":{"enum":["resolved","version_not_found","unavailable"]}}}},"incomplete":{"type":["object","null"],"required":["error_code","reason"]}}}"#,
    },
    CommandSpec {
        name: "context work-item",
        summary: "Show a kanflow work item's context in a project: its linked merge requests and the @req tests of its acceptance criteria, each with its source (M3 X5).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--ref", "option", true, "The work item, canonical PBI-<n>."),
        ],
        examples: &["gstn context work-item --project 7 --ref PBI-43", "gstn context work-item --project 7 --ref PBI-43 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/context/work-item"],
        output: r#"{"type":"object","required":["work_item","merge_requests","merge_requests_truncated","ref","tests","test_count","incomplete"],"properties":{"work_item":{"type":"object","required":["ref","integration_id","kanflow_project_id","known","first_seen_at"]},"merge_requests":{"type":"array","items":{"type":"object","required":["iid","title","state","draft","source_branch","target_branch","link_source","linked_at","source"],"properties":{"source":{"type":"object","required":["project_id","project","version","commit","path","line"]}}}},"merge_requests_truncated":{"type":"boolean"},"ref":{"type":["object","null"]},"tests":{"type":"array","items":{"type":"object","required":["target","source"],"properties":{"source":{"type":"object","required":["project_id","project","version","commit","path","line"]}}}},"test_count":{"type":"integer"},"incomplete":{"type":["object","null"],"required":["error_code","reason"]}}}"#,
    },
    CommandSpec {
        name: "context requirement",
        summary: "Show a requirement's context: its text and document, the @req tests naming it and its state in the newest versions, each with its source (M3 X5).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--id", "option", true, "The requirement id (ZEVA-CFG-012)."),
            arg("--ref", "option", false, "A full commit id, a branch or a tag (default: the default branch)."),
        ],
        examples: &["gstn context requirement --project 7 --id ZEVA-CFG-012", "gstn context requirement --project 7 --id ZEVA-CFG-012 --ref v1.5 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/context/requirement"],
        output: r#"{"type":"object","required":["ref","requirement","document","tests","test_count","versions","versions_truncated","incomplete"],"properties":{"ref":{"type":"object","required":["type","name","full_name","commit"]},"requirement":{"type":"object","required":["id","body","priority","deprecated","source"],"properties":{"source":{"type":"object","required":["project_id","project","version","commit","path","line"]}}},"document":{"type":"object","required":["path","spec","title","status","confidential"]},"tests":{"type":"array","items":{"type":"object","required":["target","source"]}},"test_count":{"type":"integer"},"versions":{"type":"array","items":{"type":"object","required":["name","commit","status","source"],"properties":{"status":{"enum":["same","changed","absent"]}}}},"versions_truncated":{"type":"boolean"},"incomplete":{"type":["object","null"],"required":["error_code","reason"]}}}"#,
    },
    CommandSpec {
        name: "context merge-request",
        summary: "Show a merge request's review context: changed files, requirements it changes, work items and check runs on its head, each with its source (M3 X5).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--iid", "option", true, "The merge request's number in the project."),
        ],
        examples: &["gstn context merge-request --project 7 --iid 12", "gstn context merge-request --project 7 --iid 12 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/merge-requests/{iid}/context"],
        output: r#"{"type":"object","required":["merge_request","files","files_truncated","requirements","requirements_truncated","work_items","checks","withheld","incomplete"],"properties":{"merge_request":{"type":"object","required":["iid","title","description","description_truncated","state","draft","source_branch","target_branch","merge_base_sha","source"]},"files":{"type":"array","items":{"type":"object","required":["status","old_path","source"]}},"requirements":{"type":"array","items":{"type":"object","required":["id","kind","body","source"]}},"work_items":{"type":"array","items":{"type":"object","required":["ref","integration_id","kanflow_project_id","link_source","confidential","source"]}},"checks":{"type":"array","items":{"type":"object","required":["name","status","conclusion","summary","summary_truncated","source"]}},"withheld":{"type":["object","null"],"required":["error_code","documents","requirements","work_items"]},"incomplete":{"type":["object","null"],"required":["error_code","reason"]}}}"#,
    },
    CommandSpec {
        name: "mcp",
        summary: "Serve a read-only MCP server on stdin/stdout for an AI agent (tools get_work_item_context, get_requirement, search_requirements, get_trace_matrix, get_merge_request_context), with your token (issue it with is_agent: a person's ordinary token is a person's); confidential items are never returned (M3 X5).",
        args: &[],
        examples: &["gstn mcp", "claude mcp add gitstation -- gstn mcp"],
        scopes: &["read_api"],
        routes: &[
            "GET /api/v1/projects/{id}/context/work-item",
            "GET /api/v1/projects/{id}/context/requirement",
            "GET /api/v1/projects/{id}/specs/requirements",
            "GET /api/v1/projects/{id}/specs/trace",
            "GET /api/v1/projects/{id}/merge-requests/{iid}/context",
        ],
        output: r#"{"type":"null","description":"JSON-RPC 2.0 messages, one per line (the MCP stdio transport); no --json output"}"#,
    },
    CommandSpec {
        name: "user rename",
        summary: "Rename a user and their personal namespace (administrators). Needs --yes.",
        args: &[
            arg("id", "positional", true, "The user id."),
            arg("--username", "option", true, "The new username (also the personal namespace path)."),
            arg("--reclaim", "flag", false, "Take back an old username of this very user."),
            arg("--yes", "flag", true, "Confirm (gstn never prompts)."),
        ],
        examples: &["gstn user rename 12 --username robert --yes"],
        scopes: &["api"],
        routes: &["POST /api/v1/users/{id}/rename"],
        output: MOVE_SCHEMA,
    },
    CommandSpec {
        name: "user search",
        summary: "Find active users by username prefix or display name (names only, at most 20; 2..=64 characters).",
        args: &[arg("term", "positional", true, "A username prefix or part of a display name.")],
        examples: &["gstn user search ali --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/users"],
        output: r#"{"type":"object","required":["items"],"properties":{"items":{"type":"array","items":{"type":"object","required":["id","username","display_name"]}}}}"#,
    },
    CommandSpec {
        name: "user list",
        summary: "List every user with its state (administrators; one page, --all follows next_cursor).",
        args: &[
            arg("--limit", "option", false, "Rows per page, 1..=100 (server default 50; 100 with --all)."),
            arg("--cursor", "option", false, "Continue after this next_cursor (the same --state; not with --all)."),
            arg("--all", "flag", false, "Follow next_cursor and merge every page (bounded)."),
            arg("--state", "option", false, "active or disabled (default: every user)."),
        ],
        examples: &["gstn user list --state disabled --json", "gstn user list --all"],
        scopes: &["read_api", "admin"],
        routes: &["GET /api/v1/admin/users"],
        output: r#"{"type":"object","required":["items","next_cursor","has_more"],"properties":{"items":{"type":"array","items":{"type":"object","required":["id","kind","username","display_name","state","is_admin","created_at"]}},"next_cursor":{"type":["string","null"]},"has_more":{"type":"boolean"}}}"#,
    },
    CommandSpec {
        name: "user disable",
        summary: "Disable a user (administrators; never past the last administrator or a last Owner, FG-AUTH-0009). Its tokens and sessions end. Needs --yes.",
        args: &[
            arg("id", "positional", true, "The user id."),
            arg("--yes", "flag", true, "Confirm (gstn never prompts)."),
        ],
        examples: &["gstn user disable 12 --yes"],
        scopes: &["api", "admin"],
        routes: &["POST /api/v1/admin/users/{id}/disable"],
        output: r#"{"type":"object","required":["user_id","outcome"],"properties":{"user_id":{"type":"integer"},"outcome":{"enum":["disabled","already_disabled"]}}}"#,
    },
    CommandSpec {
        name: "user sessions",
        summary: "List a user's active web sessions (administrators).",
        args: &[arg("id", "positional", true, "The user id.")],
        examples: &["gstn user sessions 12 --json"],
        scopes: &["read_api", "admin"],
        routes: &["GET /api/v1/admin/users/{id}/sessions"],
        output: r#"{"type":"object","required":["items","truncated"],"properties":{"items":{"type":"array","items":{"type":"object","required":["public_id","created_at","last_seen_at","idle_expires_at","absolute_expires_at","oidc","current"]}},"truncated":{"type":"boolean"}}}"#,
    },
    CommandSpec {
        name: "user end-sessions",
        summary: "End every web session of a user (administrators). Needs --yes.",
        args: &[
            arg("id", "positional", true, "The user id."),
            arg("--yes", "flag", true, "Confirm (gstn never prompts)."),
        ],
        examples: &["gstn user end-sessions 12 --yes"],
        scopes: &["api", "admin"],
        routes: &["DELETE /api/v1/admin/users/{id}/sessions"],
        output: r#"{"type":"object","required":["user_id","revoked_sessions"],"properties":{"user_id":{"type":"integer"},"revoked_sessions":{"type":"integer"}}}"#,
    },
    CommandSpec {
        name: "token list",
        summary: "List your personal access tokens (never a secret; revoked and expired ones too).",
        args: &[],
        examples: &["gstn token list --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/user/tokens"],
        output: r#"{"type":"object","required":["items"],"properties":{"items":{"type":"array","items":{"type":"object","required":["id","public_id","name","scopes","created_at","expires_at","revoked_at","last_used_at"]}}}}"#,
    },
    CommandSpec {
        name: "token revoke",
        summary: "Revoke one of your tokens (it may be the one gstn uses). Needs --yes.",
        args: &[
            arg("id", "positional", true, "The token id (token list)."),
            arg("--yes", "flag", true, "Confirm (gstn never prompts)."),
        ],
        examples: &["gstn token revoke 31 --yes"],
        scopes: &["api"],
        routes: &["DELETE /api/v1/user/tokens/{id}"],
        output: r#"{"type":"object","required":["id","revoked"],"properties":{"id":{"type":"integer"},"revoked":{"const":true}}}"#,
    },
    // ── M3 N1: your notification settings ──
    CommandSpec {
        name: "notifications get",
        summary: "Show your notification settings: mail on or off, the language of the mail (null: the instance default), the kinds muted, in-app on or off and its muted kinds; kinds lists every kind there is.",
        args: &[],
        examples: &["gstn notifications get --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/user/notification-preferences"],
        output: r#"{"type":"object","required":["email_enabled","language","muted_kinds","in_app_enabled","in_app_muted_kinds","kinds"],"properties":{"email_enabled":{"type":"boolean"},"language":{"type":["string","null"]},"muted_kinds":{"type":"array","items":{"type":"string"}},"in_app_enabled":{"type":"boolean"},"in_app_muted_kinds":{"type":"array","items":{"type":"string"}},"kinds":{"type":"array","items":{"type":"string"}}}}"#,
    },
    CommandSpec {
        name: "notifications set",
        summary: "Change your notification settings; a flag changes only what it names (the rest is kept). Nobody else's can be changed, administrators included.",
        args: &[
            arg("--email", "option", false, "on | off: all mail."),
            arg("--language", "option", false, "ja | en | id | default (the instance's)."),
            arg("--mute", "option", false, "A kind to stop (repeatable; notifications get lists them)."),
            arg("--unmute", "option", false, "A kind to receive again (repeatable)."),
            arg("--in-app", "option", false, "on | off: all in-app notifications."),
        ],
        examples: &["gstn notifications set --email off", "gstn notifications set --mute mr_commented --language en"],
        scopes: &["read_api", "api"],
        routes: &["GET /api/v1/user/notification-preferences", "PUT /api/v1/user/notification-preferences"],
        output: r#"{"type":"object","required":["email_enabled","language","muted_kinds","in_app_enabled","in_app_muted_kinds","kinds"],"properties":{"email_enabled":{"type":"boolean"},"language":{"type":["string","null"]},"muted_kinds":{"type":"array","items":{"type":"string"}},"in_app_enabled":{"type":"boolean"},"in_app_muted_kinds":{"type":"array","items":{"type":"string"}},"kinds":{"type":"array","items":{"type":"string"}}}}"#,
    },
    // ── M3 U4: your in-app notifications ──
    CommandSpec {
        name: "notifications list",
        summary: "List your in-app notifications, newest first: only those whose merge request you may still read (a page may hold fewer than --limit); next_before pages back.",
        args: &[
            arg("--unread", "flag", false, "Only unread ones."),
            arg("--before", "option", false, "Only older than this id (next_before of the previous page)."),
            arg("--limit", "option", false, "Rows per page, 1..=100 (default 20)."),
        ],
        examples: &["gstn notifications list --unread --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/user/notifications"],
        output: r#"{"type":"object","required":["items","next_before"],"properties":{"items":{"type":"array","items":{"type":"object","required":["id","kind","project_id","project_path","merge_request_iid","merge_request_title","actor_name","created_at","read_at"]}},"next_before":{"type":["integer","null"]}}}"#,
    },
    CommandSpec {
        name: "notifications count",
        summary: "How many of your in-app notifications are unread (those you may still see; at most 100 counted, capped says when more exist).",
        args: &[],
        examples: &["gstn notifications count --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/user/notifications/unread-count"],
        output: r#"{"type":"object","required":["count","capped"],"properties":{"count":{"type":"integer"},"capped":{"type":"boolean"}}}"#,
    },
    CommandSpec {
        name: "notifications read",
        summary: "Mark one of your in-app notifications read, or every unread one with --all (somebody else's is not found).",
        args: &[
            arg("id", "positional", false, "The notification id (notifications list); or --all."),
            arg("--all", "flag", false, "Every unread one."),
        ],
        examples: &["gstn notifications read 42", "gstn notifications read --all"],
        scopes: &["api"],
        routes: &["POST /api/v1/user/notifications/{id}/read", "POST /api/v1/user/notifications/read-all"],
        output: r#"{"type":"object","properties":{"id":{"type":"integer"},"read_at":{"type":"string"},"marked":{"type":"integer"}}}"#,
    },
    // ── M3 S0: service accounts (create and issue: the browser only) ──
    CommandSpec {
        name: "admin service-account list",
        summary: "List every service account with its owner, state and token summary (administrators; at most 500, truncated says when more exist).",
        args: &[],
        examples: &["gstn admin service-account list --json"],
        scopes: &["read_api", "admin"],
        routes: &["GET /api/v1/admin/service-accounts"],
        output: r#"{"type":"object","required":["items","truncated"],"properties":{"items":{"type":"array","items":{"type":"object","required":["id","username","display_name","state","owner","purpose","created_by","created_at","active_tokens","last_token_expiry"]}},"truncated":{"type":"boolean"}}}"#,
    },
    CommandSpec {
        name: "admin service-account view",
        summary: "Show one service account: owner, purpose, active tokens and the latest expiry (administrators; a person's id is not found).",
        args: &[arg("id", "positional", true, "The service account's user id.")],
        examples: &["gstn admin service-account view 42 --json"],
        scopes: &["read_api", "admin"],
        routes: &["GET /api/v1/admin/service-accounts/{id}"],
        output: SA_SCHEMA,
    },
    CommandSpec {
        name: "admin service-account update",
        summary: "Change a service account's display name, purpose or owner (administrators, people only; offboarding hands it on with --owner).",
        args: &[
            arg("id", "positional", true, "The service account's user id."),
            arg("--display-name", "option", false, "The new display name (1..=255 characters)."),
            arg("--purpose", "option", false, "Why it exists (1..=500 characters, one line)."),
            arg("--owner", "option", false, "The new owner's user id (an active person)."),
        ],
        examples: &["gstn admin service-account update 42 --owner 7", "gstn admin service-account update 42 --purpose \"Kanflow sync\" --json"],
        scopes: &["api", "admin"],
        routes: &["PATCH /api/v1/admin/service-accounts/{id}"],
        output: r#"{"type":"object","required":["outcome","fields","service_account"],"properties":{"outcome":{"enum":["updated","unchanged"]},"fields":{"type":"array","items":{"enum":["display_name","purpose","owner"]}},"service_account":{"type":"object"}}}"#,
    },
    CommandSpec {
        name: "admin service-account tokens",
        summary: "List a service account's tokens with who issued them (administrators; never a secret; revoked and expired ones too).",
        args: &[arg("id", "positional", true, "The service account's user id.")],
        examples: &["gstn admin service-account tokens 42 --json"],
        scopes: &["read_api", "admin"],
        routes: &["GET /api/v1/admin/service-accounts/{id}/tokens"],
        output: r#"{"type":"object","required":["items"],"properties":{"items":{"type":"array","items":{"type":"object","required":["id","public_id","name","scopes","created_at","expires_at","revoked_at","last_used_at","issued_by_user_id"]}}}}"#,
    },
    CommandSpec {
        name: "admin service-account revoke-token",
        summary: "Revoke one of a service account's tokens at once, e.g. after a leak (administrators; an admin-scoped token works). Needs --yes.",
        args: &[
            arg("id", "positional", true, "The service account's user id."),
            arg("token_id", "positional", true, "The token id (admin service-account tokens)."),
            arg("--yes", "flag", true, "Confirm (gstn never prompts)."),
        ],
        examples: &["gstn admin service-account revoke-token 42 31 --yes"],
        scopes: &["api", "admin"],
        routes: &["DELETE /api/v1/admin/service-accounts/{id}/tokens/{token_id}"],
        output: r#"{"type":"object","required":["service_account_id","id","revoked"],"properties":{"service_account_id":{"type":"integer"},"id":{"type":"integer"},"revoked":{"const":true}}}"#,
    },
    // ── M3 I1a: integrations (create and change: the browser only) ──
    CommandSpec {
        name: "admin integration list",
        summary: "List every integration with its kind, service account, state and binding count (administrators, people only; at most 500, truncated says when more exist).",
        args: &[],
        examples: &["gstn admin integration list --json"],
        scopes: &["read_api", "admin"],
        routes: &["GET /api/v1/admin/integrations"],
        output: r#"{"type":"object","required":["items","truncated"],"properties":{"items":{"type":"array","items":{"type":"object","required":["id","kind","service_account","enabled","event_types","bindings","created_by","created_at","updated_at"]}},"truncated":{"type":"boolean"}}}"#,
    },
    CommandSpec {
        name: "admin integration view",
        summary: "Show one integration: kind, service account, enabled, event type allowlist, binding count (administrators, people only).",
        args: &[arg("id", "positional", true, "The integration id.")],
        examples: &["gstn admin integration view 3 --json"],
        scopes: &["read_api", "admin"],
        routes: &["GET /api/v1/admin/integrations/{id}"],
        output: r#"{"type":"object","required":["id","kind","service_account","enabled","event_types","bindings","created_by","created_at","updated_at"],"properties":{"id":{"type":"integer"},"kind":{"enum":["kanflow"]},"service_account":{"type":"object","required":["id","username"]},"enabled":{"type":"boolean"},"event_types":{"type":"array","items":{"type":"string"}},"bindings":{"type":"integer"},"created_by":{"type":["integer","null"]},"created_at":{"type":"string"},"updated_at":{"type":"string"}}}"#,
    },
    CommandSpec {
        name: "admin integration bindings",
        summary: "List the projects bound to an integration with their external project ids (administrators, people only; at most 200).",
        args: &[arg("id", "positional", true, "The integration id.")],
        examples: &["gstn admin integration bindings 3 --json"],
        scopes: &["read_api", "admin"],
        routes: &["GET /api/v1/admin/integrations/{id}/bindings"],
        output: BINDINGS_SCHEMA,
    },
    CommandSpec {
        name: "integration events",
        summary: "One page of an integration's event feed after a cursor: every integration-class event of its bound projects in order, each with its work items (the integration's own service account token, read_api; FG-INT-0007 410 with reason when the cursor cannot continue, FG-INT-0008 for a cursor that is not this integration's).",
        args: &[
            arg("id", "positional", true, "The integration id."),
            arg("--cursor", "option", false, "The cursor the feed gave (next_cursor, an event's cursor, or integration cursor); without it the oldest retained event."),
            arg("--limit", "option", false, "Events per page, 1 to 500 (default 100)."),
        ],
        examples: &["gstn integration events 3 --cursor <next_cursor> --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/integrations/{id}/events"],
        output: r#"{"type":"object","required":["schema_version","events","next_cursor","has_more"],"properties":{"schema_version":{"const":1},"events":{"type":"array","items":{"type":"object","required":["id","cursor","type","schema_version","occurred_at","project_id","actor","work_items","data"]}},"next_cursor":{"type":"string"},"has_more":{"type":"boolean"}}}"#,
    },
    CommandSpec {
        name: "integration cursor",
        summary: "The head of an integration's event feed as a cursor (a resync takes it first, then the development status, then reads from it; the integration's own service account token).",
        args: &[arg("id", "positional", true, "The integration id.")],
        examples: &["gstn integration cursor 3 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/integrations/{id}/events/cursor"],
        output: r#"{"type":"object","required":["schema_version","cursor"],"properties":{"schema_version":{"const":1},"cursor":{"type":"string"}}}"#,
    },
    CommandSpec {
        name: "integration dev-status",
        summary: "One work item's development status (M3 I1d): the merge requests linked to PBI-<n> across the integration's bound projects — open, merged and closed — with approvals, the head's checks (checks_state none is no run, never success) and as_of_seq / feed_cursor (the integration's own service account token, read_api; FG-REPO-0005 for a reference of no bound kanflow project).",
        args: &[
            arg("id", "positional", true, "The integration id."),
            arg("--ref", "option", true, "The work item, PBI-<n>."),
            arg("--project-id", "option", false, "The kanflow project id of the binding (default: every bound one)."),
            arg("--state", "option", false, "all (default), open, merged or closed."),
        ],
        examples: &["gstn integration dev-status 3 --ref PBI-12 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/integrations/{id}/work-items/{ref}/dev-status"],
        output: r#"{"type":"object","required":["schema_version","as_of_seq","feed_cursor","work_items"],"properties":{"schema_version":{"const":1},"work_items":{"type":"array","items":{"type":"object","required":["provider","project_id","ref","merge_requests","merge_requests_truncated"]}}}}"#,
    },
    CommandSpec {
        name: "integration dev-status-list",
        summary: "One page of every work item's development status (M3 I1d; the resync after a 410: integration cursor first, then these pages until next_cursor is null, then integration events from that cursor or feed_cursor; FG-INT-0007 410 when the walk cannot continue).",
        args: &[
            arg("id", "positional", true, "The integration id."),
            arg("--cursor", "option", false, "The previous page's next_cursor."),
            arg("--limit", "option", false, "Work items per page, 1 to 100 (default 50)."),
            arg("--state", "option", false, "all (default), open, merged or closed."),
        ],
        examples: &["gstn integration dev-status-list 3 --cursor <next_cursor> --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/integrations/{id}/dev-status"],
        output: r#"{"type":"object","required":["schema_version","as_of_seq","feed_cursor","work_items","next_cursor","has_more"],"properties":{"schema_version":{"const":1},"next_cursor":{"type":["string","null"]},"has_more":{"type":"boolean"}}}"#,
    },
    CommandSpec {
        name: "integration-binding list",
        summary: "Show a project's integration binding, none or one (as reading the project).",
        args: &[arg("--project", "option", true, "The project id.")],
        examples: &["gstn integration-binding list --project 7 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/integration-bindings"],
        output: BINDINGS_SCHEMA,
    },
    CommandSpec {
        name: "integration-binding set",
        summary: "Bind a project to an external (kanflow) project of an integration (people only; Maintainer). Its service account becomes a Reporter of the project; a project already bound elsewhere is FG-INT-0016, a 201st binding of the integration FG-INT-0012. Binding again to the same external project answers unchanged.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--integration", "option", true, "The integration id (admin integration list)."),
            arg("--external-project", "option", true, "The external project's id (A-Z a-z 0-9 . _ : -, at most 100)."),
        ],
        examples: &["gstn integration-binding set --project 7 --integration 3 --external-project kanflow:42 --json"],
        scopes: &["api"],
        routes: &["PUT /api/v1/projects/{id}/integration-bindings/{integration_id}"],
        output: r#"{"type":"object","required":["outcome","binding"],"properties":{"outcome":{"enum":["bound","unchanged"]},"binding":{"type":"object","required":["integration_id","kind","project_id","external_project_id","granted_membership","created_by","created_at"]}}}"#,
    },
    CommandSpec {
        name: "integration-binding remove",
        summary: "Unbind a project from an integration (people only; Maintainer); the Reporter membership the binding granted is removed. Needs --yes.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--integration", "option", true, "The integration id."),
            arg("--yes", "flag", true, "Confirm (gstn never prompts)."),
        ],
        examples: &["gstn integration-binding remove --project 7 --integration 3 --yes"],
        scopes: &["api"],
        routes: &["DELETE /api/v1/projects/{id}/integration-bindings/{integration_id}"],
        output: r#"{"type":"object","required":["project_id","integration_id","unbound"],"properties":{"project_id":{"type":"integer"},"integration_id":{"type":"integer"},"unbound":{"const":true}}}"#,
    },
    CommandSpec {
        name: "admin service-account check-grants",
        summary: "The check namespaces a service account may report, per project (M3 C1; administrators).",
        args: &[arg("id", "positional", true, "The service account's user id.")],
        examples: &["gstn admin service-account check-grants 42 --json"],
        scopes: &["read_api", "admin"],
        routes: &["GET /api/v1/admin/service-accounts/{id}/check-grants"],
        output: r#"{"type":"object","required":["grants"],"properties":{"grants":{"type":"array","items":{"type":"object","required":["project_id","namespace"]}}}}"#,
    },
    CommandSpec {
        name: "admin service-account grant-checks",
        summary: "Let a service account report checks of a namespace (ci or kanflow) on one project (administrators, people only). A grant is for that project only, and the account must also be a Developer member there; forge/ and ext/ are never granted.",
        args: &[
            arg("id", "positional", true, "The service account's user id."),
            arg("--project", "option", true, "The project the grant is for."),
            arg("namespace", "positional", true, "ci or kanflow."),
        ],
        examples: &["gstn admin service-account grant-checks 42 --project 7 ci"],
        scopes: &["api", "admin"],
        routes: &["PUT /api/v1/admin/service-accounts/{id}/check-grants/{project_id}/{namespace}"],
        output: r#"{"type":"object","required":["grants","changed"],"properties":{"grants":{"type":"array","items":{"type":"object","required":["project_id","namespace"]}},"changed":{"type":"boolean"}}}"#,
    },
    CommandSpec {
        name: "admin service-account revoke-checks",
        summary: "Stop a service account reporting checks of a namespace on one project (administrators, people only); its reported runs stay.",
        args: &[
            arg("id", "positional", true, "The service account's user id."),
            arg("--project", "option", true, "The project the grant is for."),
            arg("namespace", "positional", true, "ci or kanflow."),
        ],
        examples: &["gstn admin service-account revoke-checks 42 --project 7 ci"],
        scopes: &["api", "admin"],
        routes: &["DELETE /api/v1/admin/service-accounts/{id}/check-grants/{project_id}/{namespace}"],
        output: r#"{"type":"object","required":["grants","changed"],"properties":{"grants":{"type":"array","items":{"type":"object","required":["project_id","namespace"]}},"changed":{"type":"boolean"}}}"#,
    },
    CommandSpec {
        name: "checks report",
        summary: "Report a check run on a commit (M3 C1, docs/api/checks.md): a service account holding the name's namespace (ci/, kanflow/) and a Developer member; the same name on the same commit updates its run (there is no separate update). A conclusion exactly with --status completed. The commit must be reachable from a branch or tag (otherwise not found, FG-REPO-0023). A required check of a protected branch is met only by completed/success on the merge request's head.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--sha", "option", true, "The full commit id (40 or 64 hex digits)."),
            arg("--name", "option", true, "<namespace>/<name>, e.g. ci/gates (keep it stable per kind of check)."),
            arg("--status", "option", true, "queued, in_progress or completed."),
            arg("--conclusion", "option", false, "With completed: success, neutral, failure, error or action_required."),
            arg("--summary", "option", false, "The summary (plain text / Markdown)."),
            arg("--summary-file", "option", false, "The summary from a UTF-8 file (at most 64 KiB)."),
            arg("--details-url", "option", false, "An http(s) link for people (the server never fetches it)."),
            arg("--annotations-file", "option", false, "A JSON array of {path, start_line, end_line?, level, message, title?} (at most 50)."),
        ],
        examples: &["gstn checks report --project 7 --sha 0123456789abcdef0123456789abcdef01234567 --name ci/gates --status completed --conclusion success --details-url https://ci.example.test/runs/1"],
        scopes: &["api"],
        routes: &["POST /api/v1/projects/{id}/commits/{sha}/checks"],
        output: CHECK_RUN_SCHEMA,
    },
    CommandSpec {
        name: "checks list",
        summary: "Every check run of a commit, by name, with its annotations (as the repository browse API; schema_version 1).",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--sha", "option", true, "The full commit id (40 or 64 hex digits)."),
        ],
        examples: &["gstn checks list --project 7 --sha 0123456789abcdef0123456789abcdef01234567 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/commits/{sha}/checks"],
        output: r#"{"type":"object","required":["schema_version","head_sha","check_runs"],"properties":{"schema_version":{"const":1},"head_sha":{"type":"string"},"check_runs":{"type":"array","items":{"type":"object","required":["id","name","status","conclusion","reported_by"]}}}}"#,
    },
    // ── M3 I2: project webhooks (docs/api/webhooks.md) ──
    CommandSpec {
        name: "webhook list",
        summary: "A project's webhooks (Maintainer): URL, event types, enabled, the failure count; never a secret.",
        args: &[arg("--project", "option", true, "The project id.")],
        examples: &["gstn webhook list --project 7 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/webhooks"],
        output: r#"{"type":"object","required":["items"],"properties":{"items":{"type":"array","items":{"type":"object","required":["id","project_id","owner_id","origin","event_types","enabled","secret_version","consecutive_failures"]}}}}"#,
    },
    CommandSpec {
        name: "webhook event-types",
        summary: "The event types a webhook may name, each with its class (integration, maintainer, deployment), and per class whether you may subscribe (Maintainer).",
        args: &[arg("--project", "option", true, "The project id.")],
        examples: &["gstn webhook event-types --project 7 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/webhooks/event-types"],
        output: r#"{"type":"object","required":["items","classes"],"properties":{"items":{"type":"array","items":{"type":"object","required":["name","class"]}},"classes":{"type":"array","items":{"type":"object","required":["class","subscribable"]}}}}"#,
    },
    CommandSpec {
        name: "webhook view",
        summary: "One webhook of a project (Maintainer); disabled_reason says why it is off (manual, failures, failing_24h).",
        args: &[
            arg("id", "positional", true, "The webhook id."),
            arg("--project", "option", true, "The project id."),
        ],
        examples: &["gstn webhook view 3 --project 7"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/webhooks/{webhook_id}"],
        output: r#"{"type":"object","required":["id","project_id","owner_id","origin","event_types","enabled","secret_version","consecutive_failures"]}"#,
    },
    CommandSpec {
        name: "webhook create",
        summary: "Create a webhook owned by you (a person; Maintainer). The URL must be https and on the operator's allowlist (400 FG-INT-0010 otherwise; loopback, link-local and metadata addresses never). The secret is shown once: give it to the receiver, which verifies Forge-Signature.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--url", "option", true, "The receiver's https URL."),
            arg("--event", "option", false, "An event type, a family (merge_request.*) or * (repeatable; default *)."),
        ],
        examples: &["gstn webhook create --project 7 --url https://kanflow.internal/hooks/forge --event 'merge_request.*'"],
        scopes: &["api"],
        routes: &["POST /api/v1/projects/{id}/webhooks"],
        output: r#"{"type":"object","required":["webhook","secret"]}"#,
    },
    CommandSpec {
        name: "webhook update",
        summary: "Change a webhook's URL or event types (replaced), or enable / disable it; --enable clears the failure count (a person; Maintainer).",
        args: &[
            arg("id", "positional", true, "The webhook id."),
            arg("--project", "option", true, "The project id."),
            arg("--url", "option", false, "A new https URL (the allowlist again)."),
            arg("--event", "option", false, "The new event types (repeatable)."),
            arg("--enable", "flag", false, "Enable it again."),
            arg("--disable", "flag", false, "Disable it."),
        ],
        examples: &["gstn webhook update 3 --project 7 --enable"],
        scopes: &["api"],
        routes: &["PATCH /api/v1/projects/{id}/webhooks/{webhook_id}"],
        output: r#"{"type":"object","required":["outcome","fields","webhook"]}"#,
    },
    CommandSpec {
        name: "webhook delete",
        summary: "Delete a webhook and its delivery history (a person; Maintainer). Needs --yes.",
        args: &[
            arg("id", "positional", true, "The webhook id."),
            arg("--project", "option", true, "The project id."),
            arg("--yes", "flag", true, "Confirm."),
        ],
        examples: &["gstn webhook delete 3 --project 7 --yes"],
        scopes: &["api"],
        routes: &["DELETE /api/v1/projects/{id}/webhooks/{webhook_id}"],
        output: r#"{"type":"null"}"#,
    },
    CommandSpec {
        name: "webhook rotate-secret",
        summary: "Replace a webhook's secret, shown once; the previous secret still signs for 24 hours (two v1 values in Forge-Signature) so the receiver can switch (a person; Maintainer).",
        args: &[
            arg("id", "positional", true, "The webhook id."),
            arg("--project", "option", true, "The project id."),
        ],
        examples: &["gstn webhook rotate-secret 3 --project 7"],
        scopes: &["api"],
        routes: &["POST /api/v1/projects/{id}/webhooks/{webhook_id}/secret"],
        output: r#"{"type":"object","required":["secret","secret_version","previous_secret_valid_until"]}"#,
    },
    CommandSpec {
        name: "webhook deliveries",
        summary: "A webhook's delivery history, newest first: state, attempts, the last status, reason and FG code (never a body; Maintainer).",
        args: &[
            arg("id", "positional", true, "The webhook id."),
            arg("--project", "option", true, "The project id."),
            arg("--before", "option", false, "Only deliveries older than this delivery id."),
            arg("--limit", "option", false, "1..=100 (default 20)."),
        ],
        examples: &["gstn webhook deliveries 3 --project 7 --limit 50 --json"],
        scopes: &["read_api"],
        routes: &["GET /api/v1/projects/{id}/webhooks/{webhook_id}/deliveries"],
        output: r#"{"type":"object","required":["items","next_before"]}"#,
    },
    CommandSpec {
        name: "webhook redeliver",
        summary: "Deliver one delivery again with the same Forge-Delivery id (a person; Maintainer). 429 while it is pending or within 60 s of its last attempt.",
        args: &[
            arg("id", "positional", true, "The webhook id."),
            arg("delivery", "positional", true, "The delivery id."),
            arg("--project", "option", true, "The project id."),
        ],
        examples: &["gstn webhook redeliver 3 12 --project 7"],
        scopes: &["api"],
        routes: &["POST /api/v1/projects/{id}/webhooks/{webhook_id}/deliveries/{delivery_id}/redeliver"],
        output: r#"{"type":"object","required":["outcome"]}"#,
    },
    // ── M3 O2: the push mirror to GitLab (runbook gitlab-mirror) ──
    CommandSpec {
        name: "mirror show",
        summary: "A project's push mirror to GitLab (administrators; a token needs admin): destination, credential name (never its content), enabled, and the progress — pending_since, the last success, failures and the last FG code and reason. 404 FG-REPO-0005 when there is none.",
        args: &[arg("--project", "option", true, "The project id.")],
        examples: &["gstn mirror show --project 7 --json"],
        scopes: &["read_api", "admin"],
        routes: &["GET /api/v1/projects/{id}/push-mirror"],
        output: r#"{"type":"object","required":["repo_id","url","origin","credential_name","enabled","created_by","created_at","updated_at","status"],"properties":{"status":{"type":"object","required":["pending_seq","pending_since","mirrored_seq","last_attempt_at","last_success_at","failures","last_error_code","last_reason"]}}}"#,
    },
    CommandSpec {
        name: "mirror set",
        summary: "Set a project's push mirror (an administrator, a person). The URL must be https, on mirror.allowed_hosts and without user information; --credential names a file of mirror.credentials_dir (one <username>:<token> line, mode 0600). 400 FG-INT-0021 otherwise. A new or re-enabled mirror is pushed at once.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--url", "option", true, "The GitLab repository's https URL."),
            arg("--credential", "option", true, "The credential file's name."),
            arg("--disabled", "flag", false, "Keep the setting without pushing."),
        ],
        examples: &["gstn mirror set --project 7 --url https://gitlab.internal/ymma/app.git --credential gitlab"],
        scopes: &["api", "admin"],
        routes: &["PUT /api/v1/projects/{id}/push-mirror"],
        output: r#"{"type":"object","required":["repo_id","url","origin","credential_name","enabled","status"]}"#,
    },
    CommandSpec {
        name: "mirror remove",
        summary: "Remove a project's push mirror and its progress (an administrator, a person); GitLab keeps what it has. Needs --yes.",
        args: &[
            arg("--project", "option", true, "The project id."),
            arg("--yes", "flag", true, "Confirm."),
        ],
        examples: &["gstn mirror remove --project 7 --yes"],
        scopes: &["api", "admin"],
        routes: &["DELETE /api/v1/projects/{id}/push-mirror"],
        output: r#"{"type":"null"}"#,
    },
    CommandSpec {
        name: "mirror sync",
        summary: "Queue one push of every branch and tag to the mirror now (an administrator, a person); queued false means this request's push was already queued. 404 FG-REPO-0005 without an enabled mirror.",
        args: &[arg("--project", "option", true, "The project id.")],
        examples: &["gstn mirror sync --project 7"],
        scopes: &["api", "admin"],
        routes: &["POST /api/v1/projects/{id}/push-mirror/sync"],
        output: r#"{"type":"object","required":["queued"],"properties":{"queued":{"type":"boolean"}}}"#,
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
