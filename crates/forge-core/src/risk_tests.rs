#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

use super::*;

fn file(path: &str, status: ChangeStatus) -> ChangedFile {
    ChangedFile {
        path: path.to_owned(),
        old_path: None,
        status,
        old_mode: if status == ChangeStatus::Added {
            0
        } else {
            0o100_644
        },
        new_mode: if status == ChangeStatus::Deleted {
            0
        } else {
            0o100_644
        },
        added: Some(1),
        deleted: Some(0),
    }
}

fn rules(findings: &[Finding]) -> Vec<(&'static str, RiskLevel)> {
    findings.iter().map(|f| (f.rule, f.level)).collect()
}

fn sql(content: &str) -> Vec<Finding> {
    content_findings(ContentKind::Sql, "db/migrations/0001_x.sql", content)
}

// ── levels ──

#[test]
fn levels_order_and_the_worst_wins() {
    assert!(RiskLevel::Ok < RiskLevel::Warn && RiskLevel::Warn < RiskLevel::Stop);
    assert_eq!(level(&[]), RiskLevel::Ok);
    let warn = Finding::new("w", RiskLevel::Warn, "r");
    let stop = Finding::new("s", RiskLevel::Stop, "r");
    assert_eq!(level(&[warn.clone(), stop.clone()]), RiskLevel::Stop);
    assert_eq!(level(&[warn]), RiskLevel::Warn);
    for l in [RiskLevel::Ok, RiskLevel::Warn, RiskLevel::Stop] {
        assert_eq!(RiskLevel::parse(l.as_str()), Some(l));
    }
    assert_eq!(RiskLevel::parse("fine"), None);
}

// ── migrations by path ──

#[test]
fn an_applied_migration_that_changes_or_goes_is_a_stop() {
    for status in [
        ChangeStatus::Modified,
        ChangeStatus::Deleted,
        ChangeStatus::TypeChanged,
    ] {
        let f = file("crates/db/migrations/20260101_a.sql", status);
        assert_eq!(
            rules(&path_findings(&f)),
            vec![("applied_migration_changed", RiskLevel::Stop)],
            "{status:?}"
        );
    }
    let mut renamed = file("crates/db/migrations/20260102_b.sql", ChangeStatus::Renamed);
    renamed.old_path = Some("crates/db/migrations/20260101_a.sql".to_owned());
    assert_eq!(
        rules(&path_findings(&renamed)),
        vec![("applied_migration_changed", RiskLevel::Stop)]
    );
}

#[test]
fn a_new_migration_is_a_warn_and_other_sql_is_nothing() {
    let new = file("db/migrate/0002_c.sql", ChangeStatus::Added);
    assert_eq!(
        rules(&path_findings(&new)),
        vec![("new_migration", RiskLevel::Warn)]
    );
    assert!(path_findings(&file("queries/report.sql", ChangeStatus::Modified)).is_empty());
    assert!(path_findings(&file("src/main.rs", ChangeStatus::Modified)).is_empty());
}

// ── secrets, jobs, configuration by path ──

#[test]
fn credential_files_by_name_are_a_stop_but_examples_and_fixtures_are_not() {
    for p in [
        ".env",
        "app/.env.production",
        "home/.ssh/id_rsa",
        "keys/id_ed25519",
        "web/.htpasswd",
        "store/server.p12",
        "store/server.jks",
        ".netrc",
    ] {
        assert_eq!(
            rules(&path_findings(&file(p, ChangeStatus::Added))),
            vec![("secret_file", RiskLevel::Stop)],
            "{p}"
        );
    }
    for p in [
        ".env.example",
        "app/.env.sample",
        ".env.template",
        "tests/fixtures/id_rsa",
        "docs/id_rsa.md",
    ] {
        assert!(
            !path_findings(&file(p, ChangeStatus::Added))
                .iter()
                .any(|f| f.rule == "secret_file"),
            "{p}"
        );
    }
    // A removed secret file is no new exposure.
    assert!(path_findings(&file(".env", ChangeStatus::Deleted)).is_empty());
}

#[test]
fn a_backup_restore_or_retention_job_is_a_stop_unless_it_is_a_test_or_a_doc() {
    for p in [
        "ops/backup.sh",
        "deploy/events_retention.sh",
        "scripts/restore_db.py",
    ] {
        assert!(
            rules(&path_findings(&file(p, ChangeStatus::Modified)))
                .contains(&("data_lifecycle_job", RiskLevel::Stop)),
            "{p}"
        );
    }
    for p in [
        "docs/backup-restore.md",
        "ops/runbooks/backup.md",
        "crates/app/tests/backup.rs",
        "crates/app/src/retention_tests.rs",
    ] {
        assert!(
            !path_findings(&file(p, ChangeStatus::Modified))
                .iter()
                .any(|f| f.rule == "data_lifecycle_job"),
            "{p}"
        );
    }
}

#[test]
fn authentication_ci_deploy_and_dependency_files_are_warns() {
    let cases = [
        ("crates/forge-auth/src/authorize.rs", "auth_change"),
        ("src/permissions/matrix.rs", "auth_change"),
        ("CODEOWNERS", "auth_change"),
        (".github/workflows/ci.yml", "ci_deploy_config"),
        ("deploy/app.service", "ci_deploy_config"),
        ("Dockerfile", "ci_deploy_config"),
        (".forge/ci.yml", "ci_deploy_config"),
        ("Cargo.toml", "dependency_change"),
        ("web/package-lock.json", "dependency_change"),
        ("requirements.txt", "dependency_change"),
    ];
    for (p, rule) in cases {
        assert!(
            rules(&path_findings(&file(p, ChangeStatus::Modified)))
                .contains(&(rule, RiskLevel::Warn)),
            "{p}"
        );
    }
    // The author of a book is no authentication.
    assert!(path_findings(&file("src/author.rs", ChangeStatus::Modified)).is_empty());
    // Tests and documents do not change behaviour.
    for p in [
        "crates/forge-auth/tests/matrix.rs",
        "docs/auth.md",
        "ops/runbooks/deploy.md",
    ] {
        assert!(
            path_findings(&file(p, ChangeStatus::Modified)).is_empty(),
            "{p}"
        );
    }
}

#[test]
fn a_symlink_is_a_warn_and_a_submodule_also_a_stop() {
    let mut link = file("vendor/lib", ChangeStatus::Added);
    link.new_mode = 0o120_000;
    assert_eq!(
        rules(&path_findings(&link)),
        vec![("special_entry", RiskLevel::Warn)]
    );
    for status in [ChangeStatus::Added, ChangeStatus::Modified] {
        let mut sub = file("vendor/lib", status);
        sub.new_mode = 0o160_000;
        assert_eq!(
            rules(&path_findings(&sub)),
            vec![
                ("special_entry", RiskLevel::Warn),
                ("submodule_change", RiskLevel::Stop)
            ],
            "{status:?}"
        );
    }
    let mut gone = file("vendor/lib", ChangeStatus::Deleted);
    gone.old_mode = 0o160_000;
    assert!(path_findings(&gone).is_empty());
}

// ── which files are read ──

#[test]
fn content_is_read_for_new_migrations_and_deploy_files_only() {
    assert_eq!(
        content_kind(&file("db/migrations/1.sql", ChangeStatus::Added)),
        Some(ContentKind::Sql)
    );
    // A modified one is a stop by path already.
    assert_eq!(
        content_kind(&file("db/migrations/1.sql", ChangeStatus::Modified)),
        None
    );
    assert_eq!(
        content_kind(&file("deploy/run.sh", ChangeStatus::Modified)),
        Some(ContentKind::Deploy)
    );
    assert_eq!(
        content_kind(&file("ops/runbooks/x.md", ChangeStatus::Modified)),
        None
    );
    assert_eq!(content_kind(&file("src/a.rs", ChangeStatus::Added)), None);
}

// ── SQL ──

#[test]
fn destructive_statements_are_stops_with_their_line() {
    let cases: &[(&str, &str)] = &[
        ("DROP TABLE users;", "sql_drop"),
        ("drop schema app cascade;", "sql_drop"),
        ("DROP DATABASE forge;", "sql_drop"),
        ("ALTER TABLE t DROP COLUMN c;", "sql_drop"),
        ("ALTER TABLE t DROP c;", "sql_drop"),
        ("TRUNCATE forge.jobs;", "sql_truncate"),
        ("DELETE FROM t;", "sql_unbounded_write"),
        ("UPDATE t SET a = 1;", "sql_unbounded_write"),
        ("DELETE FROM t WHERE TRUE;", "sql_unbounded_write"),
        ("UPDATE t SET a = 1 WHERE 1 = 1;", "sql_unbounded_write"),
        (
            "ALTER TABLE t ALTER COLUMN c TYPE bigint;",
            "sql_column_change",
        ),
        (
            "ALTER TABLE t ALTER COLUMN c SET DATA TYPE text;",
            "sql_column_change",
        ),
        ("ALTER TABLE t RENAME COLUMN a TO b;", "sql_column_change"),
        ("ALTER TABLE t RENAME TO u;", "sql_column_change"),
        ("ALTER TABLE t DROP CONSTRAINT t_fk;", "sql_drop_constraint"),
        ("ALTER TABLE t DISABLE TRIGGER ALL;", "sql_disable_guard"),
        (
            "ALTER TABLE t DISABLE ROW LEVEL SECURITY;",
            "sql_disable_guard",
        ),
    ];
    for (statement, rule) in cases {
        let found = sql(&format!("-- header\nSELECT 1;\n{statement}\n"));
        assert_eq!(rules(&found), vec![(*rule, RiskLevel::Stop)], "{statement}");
        assert_eq!(found[0].line, Some(3), "{statement}");
        assert_eq!(found[0].path.as_deref(), Some("db/migrations/0001_x.sql"));
    }
}

#[test]
fn safe_statements_comments_and_strings_are_clean() {
    for statement in [
        "DELETE FROM t WHERE id = 1;",
        "UPDATE t SET a = 1 WHERE id = 2;",
        "ALTER TABLE t ALTER COLUMN c DROP NOT NULL;",
        "ALTER TABLE t ALTER COLUMN c DROP DEFAULT;",
        "ALTER TABLE t ADD COLUMN c int;",
        "CREATE TABLE t (id int);",
        "INSERT INTO t (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET a = 2;",
        "-- DROP TABLE t;",
        "/* TRUNCATE t; */ SELECT 1;",
        "INSERT INTO log (m) VALUES ('DROP TABLE t; DELETE FROM t');",
        "SELECT 'it''s; TRUNCATE x';",
    ] {
        assert!(
            sql(statement).is_empty(),
            "{statement}: {:?}",
            sql(statement)
        );
    }
}

#[test]
fn dollar_quoted_bodies_and_multiline_statements_are_read() {
    let body =
        "CREATE FUNCTION f() RETURNS void AS $$\nBEGIN\n  TRUNCATE t;\nEND\n$$ LANGUAGE plpgsql;\n";
    let found = sql(body);
    assert_eq!(rules(&found), vec![("sql_truncate", RiskLevel::Stop)]);
    assert_eq!(found[0].line, Some(3));
    let multi = "UPDATE t\n   SET a = 1\n WHERE id = 3;\nDELETE\n  FROM t;\n";
    let found = sql(multi);
    assert_eq!(
        rules(&found),
        vec![("sql_unbounded_write", RiskLevel::Stop)]
    );
    assert_eq!(found[0].line, Some(4));
}

#[test]
fn objects_dropped_and_grants_are_warns() {
    for (statement, rule) in [
        ("DROP INDEX t_a_idx;", "sql_drop_object"),
        ("DROP VIEW v;", "sql_drop_object"),
        ("DROP TRIGGER g ON t;", "sql_drop_object"),
        ("GRANT SELECT ON t TO reader;", "sql_privilege"),
        ("REVOKE ALL ON t FROM reader;", "sql_privilege"),
    ] {
        assert_eq!(
            rules(&sql(statement)),
            vec![(rule, RiskLevel::Warn)],
            "{statement}"
        );
    }
}

#[test]
fn the_sql_findings_are_bounded() {
    let many = "TRUNCATE t;\n".repeat(MAX_FINDINGS_PER_FILE * 3);
    assert_eq!(sql(&many).len(), MAX_FINDINGS_PER_FILE);
}

#[test]
fn a_finding_never_carries_the_matched_text() {
    let found = sql("DELETE FROM secret_customers;");
    let shown = format!("{found:?}");
    assert!(!shown.contains("secret_customers"), "{shown}");
}

// ── deploy files ──

#[test]
fn a_rollback_step_in_a_deploy_file_is_a_stop() {
    for line in [
        "sqlx migrate revert",
        "  - run: diesel migration revert",
        "rake db:rollback",
        "dropdb forge",
        "psql -c 'DROP DATABASE forge'",
    ] {
        let found = content_findings(
            ContentKind::Deploy,
            "deploy/run.sh",
            &format!("set -e\n{line}\n"),
        );
        assert_eq!(
            rules(&found),
            vec![("deploy_rollback", RiskLevel::Stop)],
            "{line}"
        );
        assert_eq!(found[0].line, Some(2), "{line}");
    }
    assert!(
        content_findings(ContentKind::Deploy, "deploy/run.sh", "sqlx migrate run\n").is_empty()
    );
}

// ── mass deletion ──

fn deleted(path: &str, lines: Option<u64>) -> ChangedFile {
    let mut f = file(path, ChangeStatus::Deleted);
    f.added = lines.map(|_| 0);
    f.deleted = lines;
    f
}

fn totals(files: u64, lines: u64) -> TreeTotals {
    TreeTotals {
        files,
        lines,
        truncated: false,
    }
}

#[test]
fn deleting_500_lines_and_30_percent_of_the_text_is_a_stop() {
    let files = [deleted("a.rs", Some(600))];
    let f = mass_deletion(&files, &totals(10, 2000)).expect("stop");
    assert_eq!((f.rule, f.level), ("mass_deletion_lines", RiskLevel::Stop));
    // Exactly 30 % counts; 29.9 % does not.
    assert!(mass_deletion(&[deleted("a.rs", Some(600))], &totals(10, 2000)).is_some());
    assert!(mass_deletion(&[deleted("a.rs", Some(599))], &totals(10, 2000)).is_none());
    // A big repository: 600 lines is not 30 %.
    assert!(mass_deletion(&files, &totals(900, 100_000)).is_none());
    // 30 % of a small one, but under 500 lines.
    assert!(mass_deletion(&[deleted("a.rs", Some(499))], &totals(2, 600)).is_none());
}

#[test]
fn lines_removed_inside_modified_files_count() {
    let mut m = file("big.rs", ChangeStatus::Modified);
    m.added = Some(10);
    m.deleted = Some(700);
    assert!(mass_deletion(&[m], &totals(5, 1000)).is_some());
}

#[test]
fn deleting_20_files_and_30_percent_of_them_is_a_stop() {
    let files: Vec<ChangedFile> = (0..20)
        .map(|i| deleted(&format!("f{i}.bin"), None))
        .collect();
    let f = mass_deletion(&files, &totals(60, 0)).expect("stop");
    assert_eq!((f.rule, f.level), ("mass_deletion_files", RiskLevel::Stop));
    assert!(mass_deletion(&files, &totals(67, 0)).is_none()); // 29.8 %
    assert!(mass_deletion(&files[..19], &totals(20, 0)).is_none()); // under 20 files
}

#[test]
fn a_rename_is_no_deletion() {
    let files: Vec<ChangedFile> = (0..30)
        .map(|i| {
            let mut f = file(&format!("new/f{i}.rs"), ChangeStatus::Renamed);
            f.old_path = Some(format!("old/f{i}.rs"));
            f.added = Some(0);
            f.deleted = Some(0);
            f
        })
        .collect();
    assert!(mass_deletion(&files, &totals(30, 3000)).is_none());
}

#[test]
fn an_empty_target_never_divides_by_zero() {
    assert!(mass_deletion(&[deleted("a.rs", Some(900))], &totals(0, 0)).is_none());
}

// ── the whole ──

#[test]
fn findings_are_ordered_worst_first_and_capped_for_a_run() {
    let mut all = vec![
        Finding::new("b", RiskLevel::Warn, "r").at("z"),
        Finding::new("a", RiskLevel::Stop, "r").at("y"),
        Finding::new("c", RiskLevel::Warn, "r").at("a"),
    ];
    sort_findings(&mut all);
    let order: Vec<&str> = all.iter().map(|f| f.rule).collect();
    assert_eq!(order, ["a", "c", "b"]);
}

#[test]
fn every_rule_has_a_stable_reason() {
    for rule in RULES {
        assert!(!rule.id.is_empty() && !rule.reason.is_empty());
        let f = Finding::for_rule(rule.id).expect("known rule");
        assert_eq!(
            (f.rule, f.level, f.reason),
            (rule.id, rule.level, rule.reason)
        );
    }
    let ids: std::collections::BTreeSet<_> = RULES.iter().map(|r| r.id).collect();
    assert_eq!(ids.len(), RULES.len(), "rule ids are unique");
    assert!(Finding::for_rule("nope").is_none());
}

// ── review X7: control flow, CTEs, narrowed lifecycle jobs, unread files ──

#[test]
fn a_destructive_statement_after_control_flow_keywords_is_still_read() {
    for (body, rule) in [
        (
            "DO $$ BEGIN IF true THEN TRUNCATE t; END IF; END $$;",
            "sql_truncate",
        ),
        (
            "DO $$ BEGIN IF a THEN SELECT 1; ELSE DELETE FROM t; END IF; END $$;",
            "sql_unbounded_write",
        ),
        (
            "DO $$ BEGIN LOOP DROP TABLE t; END LOOP; END $$;",
            "sql_drop",
        ),
    ] {
        assert_eq!(rules(&sql(body)), vec![(rule, RiskLevel::Stop)], "{body}");
    }
    assert!(sql("DO $$ BEGIN IF a THEN SELECT 1; END IF; END $$;").is_empty());
}

#[test]
fn data_modifying_ctes_are_read() {
    for body in [
        "WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d;",
        "WITH u AS (UPDATE t SET a = 1 RETURNING a) SELECT * FROM u;",
    ] {
        assert_eq!(
            rules(&sql(body)),
            vec![("sql_unbounded_write", RiskLevel::Stop)],
            "{body}"
        );
    }
    for body in [
        "WITH d AS (DELETE FROM t WHERE id = 1 RETURNING *) SELECT * FROM d;",
        "WITH s AS (SELECT 1) SELECT * FROM s;",
        "SELECT * FROM t FOR UPDATE;",
        "CREATE TABLE c (id int REFERENCES t ON UPDATE CASCADE ON DELETE CASCADE);",
    ] {
        assert!(sql(body).is_empty(), "{body}: {:?}", sql(body));
    }
}

#[test]
fn lifecycle_jobs_are_scripts_and_job_files_not_application_source() {
    for p in [
        "ops/backup.sh",
        "scripts/restore_db.py",
        "deploy/retention.timer",
        "config/backup.yml",
        "ops/backup_job.ps1",
    ] {
        assert!(
            rules(&path_findings(&file(p, ChangeStatus::Modified)))
                .contains(&("data_lifecycle_job", RiskLevel::Stop)),
            "{p}"
        );
    }
    for p in [
        "crates/app/src/events_retention.rs",
        "web/src/restore.ts",
        "ops/runbooks/backup.md",
    ] {
        assert!(
            !path_findings(&file(p, ChangeStatus::Modified))
                .iter()
                .any(|f| f.rule == "data_lifecycle_job"),
            "{p}"
        );
    }
}

#[test]
fn an_unread_migration_or_deploy_file_is_a_stop_rule() {
    let f = Finding::for_rule("critical_content_unread").expect("rule");
    assert_eq!(f.level, RiskLevel::Stop);
}

// ── W15 A2: code that runs on developers' PCs (plan 16 §9 item 1) ──

fn agent_config(findings: &[Finding]) -> bool {
    findings
        .iter()
        .any(|f| f.rule == "agent_config" && f.level == RiskLevel::Stop)
}

#[test]
fn claude_hook_change_is_flagged() {
    for p in [
        ".claude/hooks/pre-bash.js",
        ".claude/settings.json",
        ".claude/agents/reviewer.md",
        ".claude/skills/x/SKILL.md",
        "services/api/.claude/settings.local.json",
    ] {
        for status in [ChangeStatus::Added, ChangeStatus::Modified] {
            assert!(
                agent_config(&path_findings(&file(p, status))),
                "{p} {status:?}"
            );
        }
    }
}

#[test]
fn mcp_json_change_is_flagged() {
    for p in [".mcp.json", "tools/.mcp.json"] {
        assert!(
            agent_config(&path_findings(&file(p, ChangeStatus::Modified))),
            "{p}"
        );
    }
}

#[test]
fn claude_md_at_any_level_is_flagged_although_it_is_a_doc() {
    for p in [
        "CLAUDE.md",
        "crates/app/CLAUDE.md",
        "docs/CLAUDE.md",
        "CLAUDE.local.md",
        "tests/CLAUDE.md",
    ] {
        assert!(
            agent_config(&path_findings(&file(p, ChangeStatus::Modified))),
            "{p}"
        );
    }
}

#[test]
fn gitstation_directory_is_flagged() {
    for p in [".gitstation/agent.lock.json", ".gitstation/parts/x.json"] {
        assert!(
            agent_config(&path_findings(&file(p, ChangeStatus::Added))),
            "{p}"
        );
    }
}

#[test]
fn deleting_agent_config_is_flagged() {
    // Removing a guard hook is as much a change to what runs as adding one.
    for p in [".claude/hooks/block-no-verify.js", ".mcp.json", "CLAUDE.md"] {
        assert!(
            agent_config(&path_findings(&file(p, ChangeStatus::Deleted))),
            "{p}"
        );
    }
}

#[test]
fn a_rename_into_or_out_of_claude_is_flagged() {
    let mut into = file(".claude/hooks/run.js", ChangeStatus::Renamed);
    into.old_path = Some("scripts/run.js".to_owned());
    assert!(agent_config(&path_findings(&into)));
    let mut out = file("scripts/run.js", ChangeStatus::Renamed);
    out.old_path = Some(".claude/hooks/run.js".to_owned());
    assert!(agent_config(&path_findings(&out)));
}

#[test]
fn agent_config_names_are_matched_whatever_their_case() {
    // Developers' PCs run on case-insensitive file systems (NTFS, APFS):
    // `.Claude/settings.json` lands in the `.claude` directory there.
    for p in [
        ".Claude/settings.json",
        ".CLAUDE/hooks/x.js",
        "claude.md",
        "app/Claude.md",
        ".MCP.json",
        ".GitStation/agent.lock.json",
    ] {
        assert!(
            agent_config(&path_findings(&file(p, ChangeStatus::Modified))),
            "{p}"
        );
    }
}

#[test]
fn unrelated_json_not_flagged() {
    for p in [
        "package.json",
        "config/app.json",
        "my.mcp.json",
        ".mcp.json.example",
        "mcp.json",
        "docs/.claude-notes.md",
        "docs/claude/setup.md",
        "src/claude.rs",
        "notes/CLAUDE.md.bak",
        "gitstation/readme.txt",
        ".claudeignore",
    ] {
        assert!(
            !path_findings(&file(p, ChangeStatus::Modified))
                .iter()
                .any(|f| f.rule == "agent_config"),
            "{p}"
        );
    }
}

// ── W15 A2 security review: names a developer's file system folds ──

#[test]
fn names_that_ntfs_or_apfs_fold_onto_agent_config_are_flagged() {
    for p in [
        ".claude./settings.json",
        ".claude /hooks/x",
        "CLAUDE.md.",
        "CLAUDE.md ",
        ".mcp.json::$DATA",
        ".claude:stream/x",
        ".cl\u{200b}aude/x",
        "\u{feff}.claude/x",
        ".mcp\u{2060}.json",
        "CLAUDE\u{200d}.md",
        // Unicode case: NTFS upper-cases the long s and the dotless i.
        ".mcp.j\u{17f}on",
        ".gitstat\u{131}on/agent.lock.json",
        // A lossy-decoded non-UTF-8 byte is ignored like the rest.
        ".cla\u{fffd}ude/hooks/x.js",
    ] {
        assert!(
            agent_config(&path_findings(&file(p, ChangeStatus::Modified))),
            "{p:?}"
        );
    }
    for p in [
        ".claude-x/settings.json",
        "x.claude/y",
        "CLAUDE.mdx",
        ".mcp.jsonl",
        "a/\u{200b}/b.txt",
    ] {
        assert!(
            !agent_config(&path_findings(&file(p, ChangeStatus::Modified))),
            "{p:?}"
        );
    }
}

#[test]
fn a_symlink_whose_target_resolves_to_agent_config_is_flagged() {
    for (link, target) in [
        ("tools/settings.json", "../.claude/settings.json"),
        ("hooks", ".claude/hooks"),
        ("a/b/c", "../../.mcp.json"),
        ("a/link", "./sub/../../CLAUDE.md"),
        ("x", "/home/dev/.claude"),
        ("y", r"C:\Users\dev\.claude\hooks"),
        // More `..` than the link is deep: what remains still names it.
        ("z", "../../.claude/settings.json"),
    ] {
        let found = symlink_findings(link, target.as_bytes());
        assert_eq!(
            rules(&found),
            vec![("agent_config", RiskLevel::Stop)],
            "{link} -> {target}"
        );
        assert_eq!(found[0].path.as_deref(), Some(link));
    }
    for (link, target) in [
        ("tools/x", "../src/main.rs"),
        ("docs/notes", ".claude-notes.md"),
        ("lib", "vendor/lib"),
    ] {
        assert!(
            symlink_findings(link, target.as_bytes()).is_empty(),
            "{link} -> {target}"
        );
    }
}

#[test]
fn the_rules_the_a2_review_added_are_stops() {
    for id in ["agent_config", "submodule_change"] {
        assert_eq!(
            Finding::for_rule(id).map(|f| f.level),
            Some(RiskLevel::Stop),
            "{id}"
        );
    }
}
