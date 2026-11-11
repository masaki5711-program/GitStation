//! `forge restore verify` / `finish` against a migrated database (M3 O1).
//! The database restore itself is pgBackRest's (not in the test
//! environment): a test makes its changes after the backup and then
//! **rewinds** them with SQL, which is what a restored backup looks like
//! to forge. Synthetic data only.

use std::fs;

use forge_core::ids::UserId;
use forge_core::Scope;
use forge_db::test_support::{assert_reason, url_as, FreshDb, PasswordFile, MAINT_PASSWORD};
use forge_errors::ErrorCode;

use super::*;
use crate::backup::db_tool::tests::FakeDb;
use crate::backup::tests::{commit, fixture, git, rid, versions, world, Fixture};

async fn finish(f: &Fixture, generation: &str, accept: bool) -> Result<String, ForgeError> {
    forge_db::sequencer::run_once(
        &f.t.db,
        forge_db::sequencer::BatchSize::MAX,
        forge_db::sequencer::AuditSequencing::Skip,
    )
    .await
    .expect("sequencer: the feed state exists");
    let pw = PasswordFile::new(MAINT_PASSWORD);
    let (url, path) = (url_as("forge_maint", &f.t.name), pw.path());
    let cfg = f.t.app_config(&[
        ("FORGE_DATABASE_MAINT_URL", url.as_str()),
        ("FORGE_DATABASE_MAINT_PASSWORD_FILE", path.as_str()),
    ]);
    finish_all(
        &f.t.db,
        &cfg.db,
        &f.prepared,
        &f.storage.root,
        &f.storage.git,
        &FinishArgs {
            generation,
            accept_db_behind: accept,
            allow_older_copy: false,
        },
        &rid("req-restore-finish"),
    )
    .await
}

/// Statements in one transaction with an audit row for each resource
/// (the rewind of a restored backup, or a change after it).
async fn audited(t: &FreshDb, resources: &[(&str, i64)], statements: &[String]) {
    let mut all: Vec<String> = resources
        .iter()
        .skip(1)
        .map(|(kind, id)| {
            format!(
                "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, \
                 resource_id, request_id) VALUES ('system', 'probe.changed', 'succeeded', \
                 '{kind}', {id}, 'r-fixture')"
            )
        })
        .collect();
    all.extend_from_slice(statements);
    t.commit_audited(resources[0], &all).await;
}

async fn human(t: &FreshDb, name: &str) -> UserId {
    forge_db::users::create(
        &t.db,
        &crate::admin_token::context(&rid(&format!("req-user-{name}"))),
        &forge_db::users::NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("user")
    .user_id
}

async fn token(t: &FreshDb, user: UserId) -> String {
    forge_db::tokens::issue(
        &t.db,
        &crate::admin_token::context(&rid("req-token")),
        &forge_db::tokens::NewToken {
            user,
            name: "cli".to_owned(),
            scopes: vec![Scope::ReadApi],
            expires_in: None,
            is_agent: false,
        },
    )
    .await
    .expect("token")
    .token
    .reveal()
}

async fn authenticates(t: &FreshDb, token: &str) -> bool {
    forge_db::tokens::authenticate(&t.db, &rid("req-auth"), token)
        .await
        .is_ok()
}

async fn scalar(t: &FreshDb, sql: &str) -> serde_json::Value {
    let rows = t.json_rows(sql).await;
    let row = rows.first().expect("a row");
    row.as_object()
        .and_then(|o| o.values().next().cloned())
        .expect("a column")
}

#[tokio::test]
async fn a_database_behind_the_repositories_is_detected() {
    let f = fixture(false).await;
    let (_, _, _, path, _) = world(&f, "fay").await;
    // Pushes the database does not know of: a third version ref, a merge
    // request without a row, and a repository directory without a row.
    let v3 = commit(&path, "topic", "synthetic: v3");
    git(&path, &["update-ref", "refs/merge-requests/1/v3", &v3]);
    git(&path, &["update-ref", "refs/merge-requests/7/v1", &v3]);
    git(&path, &["update-ref", "refs/merge-requests/7/head", &v3]);
    let orphan = f.storage.root.path_for(forge_core::RepoId::new(987_654));
    fs::create_dir_all(&orphan).expect("orphan");
    let v = verify(&f.t.db, &f.storage.root, &f.storage.git)
        .await
        .expect("verify");
    let counts = v.counts();
    assert_eq!(
        counts.get(&Kind::VersionRefUnknown),
        Some(&3),
        "{}",
        v.report()
    );
    assert_eq!(
        counts.get(&Kind::RepositoryUnknown),
        Some(&1),
        "{}",
        v.report()
    );
    assert_eq!(counts.len(), 2, "{}", v.report());
    let err = v.error(false).expect("finding");
    assert_eq!(err.code(), ErrorCode::RestoreVerificationFailed);
    let ctx = format!("{:?}", err.context());
    assert!(
        ctx.contains("version_ref_unknown") && ctx.contains("repository_unknown"),
        "{ctx}"
    );
    assert!(
        v.error(true).is_none(),
        "--accept-db-behind accepts only these"
    );
    // `finish` refuses without the flag and goes on with it.
    let done = f
        .backup(&FakeDb::default(), "req-backup-dbb")
        .await
        .expect("backup");
    begin(f.storage.root.path(), &rid("req-begin-dbb")).expect("begin");
    let err = finish(&f, &done.manifest.generation, false)
        .await
        .expect_err("db behind");
    assert_eq!(err.code(), ErrorCode::RestoreVerificationFailed);
    assert!(f.storage.root.path().join(MARKER).exists(), "still pending");
    let report = finish(&f, &done.manifest.generation, true)
        .await
        .expect("accepted");
    assert!(report.contains("version_ref_unknown: 3"), "{report}");
    f.t.drop_now().await;
}

#[tokio::test]
async fn the_merge_result_ref_of_a_known_merge_request_is_no_finding() {
    let f = fixture(false).await;
    let (_, _, _, path, shas) = world(&f, "hal").await;
    // The mergeability check's merge result (ADR-0024 Amendment
    // 2026-11-11): parents <target tip> then <head>, kept by
    // refs/merge-requests/<iid>/merge.
    let main = git(&path, &["rev-parse", "refs/heads/main"]);
    let tree = git(&path, &["hash-object", "-t", "tree", "-w", "/dev/null"]);
    let merge = git(
        &path,
        &[
            "commit-tree",
            &tree,
            "-p",
            &main,
            "-p",
            &shas[1],
            "-m",
            "synthetic: merge result",
        ],
    );
    git(
        &path,
        &["update-ref", "refs/merge-requests/1/merge", &merge],
    );
    let v = verify(&f.t.db, &f.storage.root, &f.storage.git)
        .await
        .expect("verify");
    assert!(v.findings.is_empty(), "{}", v.report());
    assert!(v.error(false).is_none(), "{}", v.report());
    // The same ref of a merge request the database does not know, and an
    // unknown ref of a known one, are still reported.
    git(
        &path,
        &["update-ref", "refs/merge-requests/9/merge", &merge],
    );
    git(
        &path,
        &["update-ref", "refs/merge-requests/1/other", &merge],
    );
    let v = verify(&f.t.db, &f.storage.root, &f.storage.git)
        .await
        .expect("verify");
    assert_eq!(counts_of(&v, Kind::VersionRefUnknown), 2, "{}", v.report());
    assert_eq!(v.findings.len(), 2, "{}", v.report());
    let details: Vec<&str> = v.findings.iter().map(|f| f.detail.as_str()).collect();
    assert!(
        details.contains(&"refs/merge-requests/9/merge")
            && details.contains(&"refs/merge-requests/1/other"),
        "{details:?}"
    );
    f.t.drop_now().await;
}

fn counts_of(v: &Verified, kind: Kind) -> usize {
    v.counts().get(&kind).copied().unwrap_or(0)
}

#[tokio::test]
async fn repositories_behind_the_database_are_detected() {
    let f = fixture(false).await;
    let (_, project, owner, path, shas) = world(&f, "gil").await;
    // A version the repositories lost, a moved version ref, and a purged
    // version whose ref came back (secret removal undone).
    let lost = commit(&path, "other", "synthetic: lost");
    versions(&f.t, project, owner, 2, &[&lost]).await;
    git(&path, &["update-ref", "refs/merge-requests/1/v1", &shas[1]]);
    audited(
        &f.t,
        &[("project", project.get())],
        &[format!(
            "UPDATE forge.merge_request_versions SET refs_purged_at = now() WHERE project_id = {} \
             AND version_no = 2 AND head_sha = '{}'",
            project.get(),
            shas[1]
        )],
    )
    .await;
    let v = verify(&f.t.db, &f.storage.root, &f.storage.git)
        .await
        .expect("verify");
    let counts = v.counts();
    assert_eq!(
        counts.get(&Kind::VersionRefMissing),
        Some(&1),
        "{}",
        v.report()
    );
    assert_eq!(
        counts.get(&Kind::VersionRefMismatch),
        Some(&1),
        "{}",
        v.report()
    );
    assert_eq!(
        counts.get(&Kind::PurgedRefPresent),
        Some(&1),
        "{}",
        v.report()
    );
    assert!(v.error(true).is_some(), "not a database-behind finding");
    f.t.drop_now().await;
}

#[tokio::test]
async fn restore_does_not_resurrect_revoked_credentials() {
    let f = fixture(true).await;
    let (_, project, owner, _, _) = world(&f, "hal").await;
    let pat = token(&f.t, owner).await;
    let other = human(&f.t, "ida").await;
    let other_pat = token(&f.t, other).await;
    // A service account's token (kanflow, ci/gates).
    let sa = forge_db::service_accounts::create(
        &f.t.db,
        &crate::admin_token::context(&rid("req-sa")),
        &forge_db::service_accounts::NewServiceAccount {
            username: "kanflow-synthetic".to_owned(),
            display_name: "Kanflow".to_owned(),
            owner,
            purpose: "synthetic test account".to_owned(),
        },
        owner,
    )
    .await
    .expect("service account")
    .user_id;
    let sa_token = forge_db::tokens::issue_for_service_account(
        &f.t.db,
        &crate::admin_token::context(&rid("req-sa-token")),
        &forge_db::tokens::NewToken {
            user: sa,
            name: "kanflow".to_owned(),
            scopes: vec![Scope::ReadApi],
            expires_in: None,
            is_agent: false,
        },
        owner,
    )
    .await
    .expect("sa token")
    .token
    .reveal();
    let session = forge_db::test_support::issue_session(&f.t.db, other).await;
    // A webhook with a signing secret (synthetic ciphertext).
    audited(
        &f.t,
        &[("project", project.get())],
        &[format!(
            "INSERT INTO forge.webhooks (uid, project_id, owner_user_id, url, origin, \
             event_types, secret_key_id, secret_ciphertext) VALUES (gen_random_uuid(), {}, {}, \
             'https://hooks.example.invalid/x', 'https://hooks.example.invalid:443', \
             ARRAY['*'], 1, '\\x{}'::bytea)",
            project.get(),
            owner.get(),
            "00".repeat(28)
        )],
    )
    .await;
    let done = f
        .backup(&FakeDb::default(), "req-backup-cr")
        .await
        .expect("backup");
    // After the backup: the personal token and the service account's token
    // are revoked, ida is disabled.
    let revoke = format!(
        "UPDATE forge.tokens SET revoked_at = now(), revoked_reason = 'revoked' WHERE user_id IN \
         ({}, {})",
        owner.get(),
        sa.get()
    );
    audited(
        &f.t,
        &[
            ("user", owner.get()),
            ("user", sa.get()),
            ("user", other.get()),
        ],
        &[
            revoke,
            format!(
                "UPDATE forge.users SET state = 'disabled' WHERE id = {}",
                other.get()
            ),
        ],
    )
    .await;
    assert!(!authenticates(&f.t, &pat).await);
    crate::state_copy::write(
        &f.t.db,
        f.prepared.state_copy_dir.as_deref().expect("dir"),
        &f.prepared.key,
    )
    .await
    .expect("state copy");
    // The restored database: as at the backup (the revocations and the
    // disable are gone).
    audited(
        &f.t,
        &[
            ("user", owner.get()),
            ("user", sa.get()),
            ("user", other.get()),
        ],
        &[
            format!(
                "UPDATE forge.tokens SET revoked_at = NULL, revoked_reason = NULL WHERE user_id \
                 IN ({}, {})",
                owner.get(),
                sa.get()
            ),
            format!(
                "UPDATE forge.users SET state = 'active' WHERE id = {}",
                other.get()
            ),
        ],
    )
    .await;
    assert!(
        authenticates(&f.t, &pat).await,
        "the restored database brought it back"
    );
    begin(f.storage.root.path(), &rid("req-begin-cr")).expect("begin");
    let report = finish(&f, &done.manifest.generation, false)
        .await
        .expect("finish");
    assert!(report.contains("disabled 1 webhook(s)"), "{report}");
    // No credential issued before the restore works, revoked or not.
    for (name, t) in [
        ("personal", &pat),
        ("other", &other_pat),
        ("service account", &sa_token),
    ] {
        assert!(!authenticates(&f.t, t).await, "{name} token resurrected");
    }
    let session_ok = forge_db::sessions::authenticate(
        &f.t.db,
        &rid("req-session"),
        &session.token.reveal(),
        std::time::Duration::from_secs(3600),
    )
    .await
    .is_ok();
    assert!(!session_ok, "the session ended");
    assert_eq!(
        scalar(
            &f.t,
            &format!("SELECT state FROM forge.users WHERE id = {}", other.get())
        )
        .await,
        "disabled",
        "the disable after the backup was re-applied"
    );
    assert_eq!(
        scalar(
            &f.t,
            "SELECT count(*) FROM forge.tokens WHERE revoked_at IS NULL"
        )
        .await,
        0
    );
    assert_eq!(
        scalar(&f.t, "SELECT disabled_reason FROM forge.webhooks").await,
        "restore"
    );
    assert_eq!(
        scalar(
            &f.t,
            "SELECT count(*) FROM forge.audit_log WHERE action = 'restore.finished' \
             AND actor_kind = 'system' AND actor_label LIKE 'forge-cli:%'"
        )
        .await,
        1
    );
    // The event feed has a new epoch: every integration's cursor answers
    // 410 epoch_mismatch (forge-integ `epoch_mismatch_is_410` reads it).
    assert_eq!(
        scalar(
            &f.t,
            "SELECT count(*) FROM forge.audit_log WHERE action = 'feed.epoch_rotated'"
        )
        .await,
        1
    );
    assert!(
        !f.storage.root.path().join(MARKER).exists(),
        "the restore ended"
    );
    refuse_if_pending(f.storage.root.path()).expect("forge serve may start");
    f.t.drop_now().await;
}

#[tokio::test]
async fn restore_reapplies_post_backup_role_changes() {
    let f = fixture(true).await;
    let (_, project, _, _, _) = world(&f, "jan").await;
    let p = project.get();
    let (dev, maint, admin) = (
        human(&f.t, "kim").await,
        human(&f.t, "lee").await,
        human(&f.t, "max").await,
    );
    let ctx = crate::admin_token::context(&rid("req-members"));
    for (u, role) in [
        (dev, forge_core::Role::Developer),
        (maint, forge_core::Role::Maintainer),
    ] {
        forge_db::projects::add_member(&f.t.db, &ctx, project, u, role)
            .await
            .expect("member");
    }
    let rule = forge_core::RefPattern::parse("refs/heads/main").expect("pattern");
    forge_db::protected_refs::create(&f.t.db, &ctx, project, &rule, forge_core::Role::Developer)
        .await
        .expect("rule");
    audited(
        &f.t,
        &[("user", admin.get())],
        &[format!(
            "UPDATE forge.users SET is_admin = true WHERE id = {}",
            admin.get()
        )],
    )
    .await;
    let done = f
        .backup(&FakeDb::default(), "req-backup-rr")
        .await
        .expect("backup");
    // After the backup: kim leaves, lee is lowered to reporter, max loses
    // the administrator flag, main needs maintainer, release/* is protected.
    let narrow = [
        format!(
            "DELETE FROM forge.project_members WHERE project_id = {p} AND user_id = {}",
            dev.get()
        ),
        format!(
            "UPDATE forge.project_members SET role = 'reporter' WHERE project_id = {p} AND \
             user_id = {}",
            maint.get()
        ),
        format!(
            "UPDATE forge.protected_refs SET push_min_role = 'maintainer' WHERE project_id = {p}"
        ),
        format!(
            "INSERT INTO forge.protected_refs (project_id, pattern, push_min_role) VALUES ({p}, \
             'refs/heads/release/*', 'owner')"
        ),
    ];
    audited(&f.t, &[("project", p)], &narrow).await;
    // A widening after the backup (ned joins) is in the copy but not in the
    // restored database: it is not re-applied (the people repeat it).
    let late = human(&f.t, "ned").await;
    forge_db::projects::add_member(&f.t.db, &ctx, project, late, forge_core::Role::Guest)
        .await
        .expect("late member");
    audited(
        &f.t,
        &[("user", admin.get())],
        &[format!(
            "UPDATE forge.users SET is_admin = false WHERE id = {}",
            admin.get()
        )],
    )
    .await;
    crate::state_copy::write(
        &f.t.db,
        f.prepared.state_copy_dir.as_deref().expect("dir"),
        &f.prepared.key,
    )
    .await
    .expect("state copy");
    // The restored database: as at the backup.
    let rewind = [
        format!(
            "INSERT INTO forge.project_members (project_id, user_id, role) VALUES ({p}, {}, \
             'developer')",
            dev.get()
        ),
        format!(
            "UPDATE forge.project_members SET role = 'maintainer' WHERE project_id = {p} AND \
             user_id = {}",
            maint.get()
        ),
        format!("UPDATE forge.protected_refs SET push_min_role = 'developer' WHERE project_id = {p}"),
        format!("DELETE FROM forge.protected_refs WHERE project_id = {p} AND pattern = 'refs/heads/release/*'"),
        format!("DELETE FROM forge.project_members WHERE project_id = {p} AND user_id = {}", late.get()),
    ];
    audited(&f.t, &[("project", p)], &rewind).await;
    audited(
        &f.t,
        &[("user", admin.get())],
        &[format!(
            "UPDATE forge.users SET is_admin = true WHERE id = {}",
            admin.get()
        )],
    )
    .await;
    begin(f.storage.root.path(), &rid("req-begin-rr")).expect("begin");
    let report = finish(&f, &done.manifest.generation, false)
        .await
        .expect("finish");
    assert!(report.contains("re-applied"), "{report}");
    let role = |u: UserId| {
        format!(
            "SELECT role FROM forge.project_members WHERE project_id = {p} AND user_id = {}",
            u.get()
        )
    };
    assert!(
        f.t.json_rows(&role(dev)).await.is_empty(),
        "the removal is back"
    );
    assert_eq!(scalar(&f.t, &role(maint)).await, "reporter");
    assert!(
        f.t.json_rows(&role(late)).await.is_empty(),
        "a widening is not re-applied"
    );
    assert_eq!(
        scalar(
            &f.t,
            &format!(
                "SELECT is_admin FROM forge.users WHERE id = {}",
                admin.get()
            )
        )
        .await,
        false
    );
    assert_eq!(
        scalar(
            &f.t,
            &format!(
                "SELECT push_min_role FROM forge.protected_refs WHERE project_id = {p} AND \
                 pattern = 'refs/heads/main'"
            )
        )
        .await,
        "maintainer"
    );
    assert_eq!(
        scalar(
            &f.t,
            &format!(
                "SELECT push_min_role FROM forge.protected_refs WHERE project_id = {p} AND \
                 pattern = 'refs/heads/release/*'"
            )
        )
        .await,
        "owner"
    );
    assert!(
        scalar(
            &f.t,
            "SELECT count(*) FROM forge.audit_log WHERE action = 'restore.reapplied'"
        )
        .await
        .as_i64()
        .is_some_and(|n| n >= 2),
        "audited per resource"
    );
    f.t.drop_now().await;
}

#[tokio::test]
async fn without_a_state_copy_every_admin_flag_is_removed_and_listed() {
    let f = fixture(false).await;
    world(&f, "oli").await;
    let (a, b) = (human(&f.t, "pam").await, human(&f.t, "quy").await);
    audited(
        &f.t,
        &[("user", a.get()), ("user", b.get())],
        &[format!(
            "UPDATE forge.users SET is_admin = true WHERE id IN ({}, {})",
            a.get(),
            b.get()
        )],
    )
    .await;
    let done = f
        .backup(&FakeDb::default(), "req-backup-na")
        .await
        .expect("backup");
    begin(f.storage.root.path(), &rid("req-begin-na")).expect("begin");
    let report = finish(&f, &done.manifest.generation, false)
        .await
        .expect("finish");
    assert!(
        report.contains("no usable state copy (not_configured)"),
        "{report}"
    );
    assert!(report.contains("pam") && report.contains("quy"), "{report}");
    assert_eq!(
        scalar(&f.t, "SELECT count(*) FROM forge.users WHERE is_admin").await,
        0
    );
    // The way back: only for a user the restore listed, and only active humans.
    regrant_admin_with(&f.t.db, "pam", &rid("req-regrant"))
        .await
        .expect("regrant");
    assert_eq!(
        scalar(
            &f.t,
            &format!("SELECT is_admin FROM forge.users WHERE id = {}", a.get())
        )
        .await,
        true
    );
    human(&f.t, "ray").await;
    let err = regrant_admin_with(&f.t.db, "ray", &rid("req-regrant-2"))
        .await
        .expect_err("not listed");
    assert_reason(
        &err,
        ErrorCode::AdministrationConflict,
        "not_removed_by_restore",
    );
    let again = regrant_admin_with(&f.t.db, "pam", &rid("req-regrant-3"))
        .await
        .expect("idempotent");
    assert!(again.contains("already"), "{again}");
    f.t.drop_now().await;
}

#[tokio::test]
async fn a_state_copy_older_than_the_backup_is_not_used() {
    let f = fixture(true).await;
    world(&f, "sue").await;
    let a = human(&f.t, "tom").await;
    audited(
        &f.t,
        &[("user", a.get())],
        &[format!(
            "UPDATE forge.users SET is_admin = true WHERE id = {}",
            a.get()
        )],
    )
    .await;
    let dir = f.prepared.state_copy_dir.clone().expect("dir"); // clone: the fixture keeps it
    let old = crate::state_copy::write(&f.t.db, &dir, &f.prepared.key)
        .await
        .expect("old copy");
    let done = f
        .backup(&FakeDb::default(), "req-backup-old")
        .await
        .expect("backup");
    // The backup wrote its own copy; take it away so only the old one is left.
    let fresh = done.state_copy.clone().expect("fresh"); // clone: removed below
    fs::remove_file(&fresh).expect("remove");
    assert!(old.exists());
    begin(f.storage.root.path(), &rid("req-begin-old")).expect("begin");
    let report = finish(&f, &done.manifest.generation, false)
        .await
        .expect("finish");
    assert!(
        report.contains("no usable state copy (none_new_enough)"),
        "{report}"
    );
    assert!(report.contains("tom"), "{report}");
    f.t.drop_now().await;
}

#[tokio::test]
async fn serve_and_worker_refuse_while_a_restore_is_pending() {
    let tmp = tempfile::tempdir().expect("tmp");
    refuse_if_pending(tmp.path()).expect("no restore");
    begin(tmp.path(), &rid("req-begin-p")).expect("begin");
    let err = refuse_if_pending(tmp.path()).expect_err("pending");
    assert_reason(
        &err,
        ErrorCode::RestoreVerificationFailed,
        "restore_pending",
    );
}

#[test]
fn forge_web_never_names_the_restore_commands() {
    // Assembled at run time so this file does not match itself.
    let forbidden = [
        format!("{}{}", "revoke_all_", "credentials"),
        format!("{}{}", "remove_all_admin_", "flags"),
        format!("{}{}", "regrant_", "admin"),
        format!("{}{}", "record_", "finished"),
        format!("{}{}", "restore::", "state"),
        format!("{}{}", "forge_db::", "restore"),
    ];
    let mut files = Vec::new();
    for dir in ["../forge-web/src", "../forge-hosting/src"] {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(dir);
        let before = files.len();
        rust_files(&src, &mut files);
        assert!(files.len() > before, "no sources under {}", src.display());
    }
    let mut offenders = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).expect("read");
        for pat in &forbidden {
            if text.contains(pat.as_str()) {
                offenders.push(format!("{}: {pat}", file.display()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "HTTP must not reach the restore commands: {offenders:?}"
    );
}

fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for e in fs::read_dir(dir).expect("read dir").filter_map(Result::ok) {
        let p = e.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[tokio::test]
async fn regrant_is_refused_after_a_later_change_of_the_user() {
    let f = fixture(false).await;
    world(&f, "zed").await;
    let (a, b) = (human(&f.t, "abe").await, human(&f.t, "bel").await);
    audited(
        &f.t,
        &[("user", a.get()), ("user", b.get())],
        &[format!(
            "UPDATE forge.users SET is_admin = true WHERE id IN ({}, {})",
            a.get(),
            b.get()
        )],
    )
    .await;
    let done = f
        .backup(&FakeDb::default(), "req-backup-lc")
        .await
        .expect("backup");
    // Before finish nothing can be regranted (no finished restore lists them).
    begin(f.storage.root.path(), &rid("req-begin-lc")).expect("begin");
    finish(&f, &done.manifest.generation, false)
        .await
        .expect("finish");
    // bel was changed after the restore listed her: the operator's check is
    // out of date.
    audited(
        &f.t,
        &[("user", b.get())],
        &[format!(
            "UPDATE forge.users SET display_name = 'synthetic' WHERE id = {}",
            b.get()
        )],
    )
    .await;
    let err = regrant_admin_with(&f.t.db, "bel", &rid("req-regrant-lc"))
        .await
        .expect_err("changed since");
    assert_reason(
        &err,
        ErrorCode::AdministrationConflict,
        "not_removed_by_restore",
    );
    regrant_admin_with(&f.t.db, "abe", &rid("req-regrant-lc2"))
        .await
        .expect("listed and unchanged");
    f.t.drop_now().await;
}

#[tokio::test]
async fn a_finish_that_stopped_half_way_runs_again_and_says_so() {
    let f = fixture(false).await;
    world(&f, "cyd").await;
    let a = human(&f.t, "dax").await;
    audited(
        &f.t,
        &[("user", a.get())],
        &[format!(
            "UPDATE forge.users SET is_admin = true WHERE id = {}",
            a.get()
        )],
    )
    .await;
    let done = f
        .backup(&FakeDb::default(), "req-backup-rr2")
        .await
        .expect("backup");
    begin(f.storage.root.path(), &rid("req-begin-rr2")).expect("begin");
    // The first run did its database steps and stopped before the epoch.
    let first = finish_with(
        &f.t.db,
        &f.prepared,
        &f.storage.root,
        &f.storage.git,
        &FinishArgs {
            generation: &done.manifest.generation,
            accept_db_behind: false,
            allow_older_copy: false,
        },
        &rid("req-finish-1"),
    )
    .await
    .expect("first run");
    assert!(!first.rerun);
    assert_eq!(first.admins_removed.len(), 1);
    assert_eq!(first.admins_left, 0);
    let report = finish(&f, &done.manifest.generation, false)
        .await
        .expect("second run");
    assert!(report.contains("re-run"), "{report}");
    assert!(
        report.contains("dax"),
        "the list comes from the first run: {report}"
    );
    assert!(
        report.contains("no active administrator is left"),
        "{report}"
    );
    let rows =
        f.t.json_rows("SELECT details FROM forge.audit_log WHERE action = 'restore.finished'")
            .await;
    assert_eq!(rows[0]["details"]["rerun"], true);
    assert_eq!(rows[0]["details"]["admin_flags_removed"], 1);
    f.t.drop_now().await;
}

#[tokio::test]
async fn an_unreadable_newest_copy_counts_as_no_copy_unless_older_ones_are_allowed() {
    let f = fixture(true).await;
    world(&f, "eli").await;
    let dir = f.prepared.state_copy_dir.clone().expect("dir"); // clone: the fixture keeps it
    let done = f
        .backup(&FakeDb::default(), "req-backup-un")
        .await
        .expect("backup");
    let good = done.state_copy.clone().expect("copy"); // clone: compared below
                                                       // A newer copy that does not open (changed, or another key).
    let bad = dir.join("state-29991231T235959Z-00000000000000000001.fgbk");
    fs::write(&bad, b"FGBK not really").expect("bad copy");
    let micros = done.manifest.db.completed_at_micros;
    let err = crate::state_copy::newest_usable(Some(&dir), &f.prepared.key, micros, false)
        .expect_err("newest unreadable");
    assert_eq!(err, crate::state_copy::Unusable::Unreadable);
    let (used, _) = crate::state_copy::newest_usable(Some(&dir), &f.prepared.key, micros, true)
        .expect("an older one is allowed");
    assert_eq!(used, good);
    f.t.drop_now().await;
}
