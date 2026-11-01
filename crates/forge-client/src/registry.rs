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

const PROJECT_FIELDS: &str = r#"{"type":"object","required":["id","namespace_id","path","name","display_name","description","visibility","kind","created_at","role"],"properties":{"id":{"type":"integer"},"namespace_id":{"type":"integer"},"path":{"type":"string"},"name":{"type":"string"},"display_name":{"type":"string"},"description":{"type":"string"},"visibility":{"enum":["private","internal"]},"kind":{"enum":["spec","system"]},"created_at":{"type":"string"},"role":{"type":["string","null"]}}}"#;
const PROJECT_SCHEMA: &str = PROJECT_FIELDS;
const PROJECT_PAGE_SCHEMA: &str = r#"{"type":"object","required":["items","next_cursor","has_more"],"properties":{"items":{"type":"array","items":{"type":"object","required":["id","namespace_id","path","name","display_name","description","visibility","kind","created_at","role"]}},"next_cursor":{"type":["string","null"]},"has_more":{"type":"boolean"}}}"#;
const GROUP_SCHEMA: &str = r#"{"type":"object","required":["id","namespace_id","parent_id","path","name","display_name","description","created_at","role"],"properties":{"id":{"type":"integer"},"namespace_id":{"type":"integer"},"parent_id":{"type":["integer","null"]},"path":{"type":"string"},"name":{"type":"string"},"display_name":{"type":"string"},"description":{"type":"string"},"created_at":{"type":"string"},"role":{"type":["string","null"]}}}"#;
const MEMBER_SCHEMA: &str = r#"{"type":"object","required":["user_id","username","display_name","role","active","created_at"],"properties":{"user_id":{"type":"integer"},"username":{"type":"string"},"display_name":{"type":"string"},"role":{"type":"string"},"active":{"type":"boolean"},"created_at":{"type":"string"}}}"#;
const MOVE_SCHEMA: &str = r#"{"type":"object","required":["id","kind","namespace_id","old_path","path","tombstones","projects","reclaimed_tombstones"],"properties":{"id":{"type":"integer"},"kind":{"enum":["project","group","user"]},"namespace_id":{"type":"integer"},"old_path":{"type":"string"},"path":{"type":"string"},"tombstones":{"type":"integer"},"projects":{"type":"integer"},"reclaimed_tombstones":{"type":"integer"}}}"#;
const PROTECTED_REF_SCHEMA: &str = r#"{"type":"object","required":["id","pattern","push_min_role","created_at","updated_at"],"properties":{"id":{"type":"integer"},"pattern":{"type":"string"},"push_min_role":{"type":"string"},"created_at":{"type":"string"},"updated_at":{"type":"string"}}}"#;

const COMMIT_DETAIL_SCHEMA: &str = r#"{"type":"object","required":["commit","message","diff"],"properties":{"commit":{"type":"object","required":["oid","parents","author_name","author_email","authored_at","committer_name","committer_email","committed_at","subject"]},"message":{"type":"string"},"diff":{"type":"object","required":["files","truncated"],"properties":{"files":{"type":"array","items":{"type":"object","required":["status","old_path","new_path","old_mode","new_mode","old_oid","new_oid","similarity","binary","patch","omitted"],"properties":{"status":{"enum":["added","modified","deleted","renamed","copied","type_changed"]},"patch":{"type":["string","null"]},"omitted":{"enum":["too_large","total_cap","not_utf8",null]}}}},"truncated":{"type":"boolean"}}}}}"#;

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
            arg("--kind", "option", false, "spec | system (server default system; fixed at creation)."),
            arg("--visibility", "option", false, "private | internal (server default private)."),
        ],
        examples: &["gstn project create --namespace-id 3 --path app --display-name 'Team app' --kind system"],
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
