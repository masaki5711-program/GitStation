//! The push record, its digest, its bounded JSON, FG-GIT-0012 and the
//! `forge.git.push_applied` log line (M1 phase 9 MR2).

use std::io::Write;
use std::sync::{Arc, Mutex};

use forge_errors::{ErrorCode, ForgeError};

use crate::push::*;
use crate::push_log::{PushApplied, PUSH_APPLIED_EVENT};
use crate::ref_protection::{ProtectionRule, ProtectionRules};
use crate::{ProjectId, RefName, RefPattern, RepoId, RequestId, Role, UserId};

fn oid(c: char) -> ObjectId {
    ObjectId::parse(&c.to_string().repeat(40)).expect("oid")
}

fn zero() -> ObjectId {
    oid('0')
}

fn name(raw: &str) -> RefName {
    RefName::parse(raw).expect("ref")
}

fn update(raw: &str, old: ObjectId, new: ObjectId, kind: PushUpdateKind) -> PushedRef {
    let commits = if kind == PushUpdateKind::Delete {
        Vec::new()
    } else {
        vec![new.clone()]
    };
    PushedRef {
        name: name(raw),
        old,
        new,
        kind,
        total_commits: commits.len() as u64,
        commits,
    }
}

fn record(updates: Vec<PushedRef>) -> PushRecord {
    PushRecord {
        repository_id: RepoId::new(7),
        updates,
    }
}

fn rid(raw: &str) -> RequestId {
    RequestId::parse(raw).expect("request id")
}

fn reason(e: &ForgeError, key: &str) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

fn rule_of(push: &PushRecord) -> String {
    let err = push.check().expect_err("refused");
    assert_eq!(err.code(), ErrorCode::ChangeRejected, "{err}");
    assert_eq!(reason(&err, "reason"), "invalid_push_record", "{err}");
    reason(&err, "rule")
}

#[test]
fn object_ids_are_40_or_64_lowercase_hex() {
    assert!(ObjectId::parse(&"a".repeat(40)).is_ok());
    assert!(ObjectId::parse(&"0123456789abcdef".repeat(4)).is_ok());
    for bad in [
        String::new(),
        "a".repeat(39),
        "a".repeat(41),
        "A".repeat(40),
        "g".repeat(40),
        "a".repeat(63),
    ] {
        let err = ObjectId::parse(&bad).expect_err("refused");
        assert_eq!(reason(&err, "rule"), "object_id");
    }
    assert!(zero().is_zero());
    assert!(!oid('a').is_zero());
}

#[test]
fn a_consistent_record_passes_the_check() {
    let push = record(vec![
        update("refs/heads/new", zero(), oid('a'), PushUpdateKind::Create),
        update(
            "refs/heads/main",
            oid('a'),
            oid('b'),
            PushUpdateKind::FastForward,
        ),
        update(
            "refs/heads/rewrite",
            oid('b'),
            oid('c'),
            PushUpdateKind::Force,
        ),
        update("refs/tags/old", oid('c'), zero(), PushUpdateKind::Delete),
    ]);
    push.check().expect("consistent");
}

#[test]
fn an_inconsistent_record_is_refused_with_its_rule() {
    assert_eq!(rule_of(&record(Vec::new())), "no_updates");
    let many = (0..=MAX_PUSH_UPDATES)
        .map(|i| {
            update(
                &format!("refs/heads/b{i}"),
                zero(),
                oid('a'),
                PushUpdateKind::Create,
            )
        })
        .collect();
    assert_eq!(rule_of(&record(many)), "too_many_updates");
    let a = update("refs/heads/a", zero(), oid('a'), PushUpdateKind::Create);
    assert_eq!(
        rule_of(&record(vec![a.clone(), a.clone()])),
        "duplicate_ref"
    );
    let sha256 = ObjectId::parse(&"b".repeat(64)).expect("oid");
    let mixed = update("refs/heads/m", oid('a'), sha256, PushUpdateKind::Force);
    assert_eq!(rule_of(&record(vec![mixed])), "mixed_hash_length");
    let same = update(
        "refs/heads/s",
        oid('a'),
        oid('a'),
        PushUpdateKind::FastForward,
    );
    assert_eq!(rule_of(&record(vec![same])), "unchanged_ref");
    let zeros = update("refs/heads/z", zero(), zero(), PushUpdateKind::Delete);
    assert_eq!(rule_of(&record(vec![zeros])), "unchanged_ref");
    for (old, new, kind) in [
        (zero(), oid('a'), PushUpdateKind::FastForward),
        (oid('a'), zero(), PushUpdateKind::Force),
        (oid('a'), oid('b'), PushUpdateKind::Create),
        (oid('a'), oid('b'), PushUpdateKind::Delete),
    ] {
        let mut u = update("refs/heads/k", old, new, kind);
        u.commits.clear();
        u.total_commits = 0;
        assert_eq!(rule_of(&record(vec![u])), "kind_mismatch", "{kind:?}");
    }
    let mut delete = update("refs/heads/d", oid('a'), zero(), PushUpdateKind::Delete);
    delete.total_commits = 1;
    assert_eq!(rule_of(&record(vec![delete])), "delete_with_commits");
    let mut long = update(
        "refs/heads/l",
        oid('a'),
        oid('b'),
        PushUpdateKind::FastForward,
    );
    long.commits = vec![oid('c'); MAX_LISTED_COMMITS + 1];
    long.total_commits = 100;
    assert_eq!(rule_of(&record(vec![long])), "too_many_commits");
    let mut fewer = update(
        "refs/heads/f",
        oid('a'),
        oid('b'),
        PushUpdateKind::FastForward,
    );
    fewer.total_commits = 0;
    assert_eq!(rule_of(&record(vec![fewer])), "total_commits");
    let mut huge = update(
        "refs/heads/h",
        oid('a'),
        oid('b'),
        PushUpdateKind::FastForward,
    );
    huge.total_commits = MAX_TOTAL_COMMITS + 1;
    assert_eq!(rule_of(&record(vec![huge])), "total_commits");
}

#[test]
fn the_digest_ignores_the_order_and_tells_requests_apart() {
    let main = update(
        "refs/heads/main",
        oid('a'),
        oid('b'),
        PushUpdateKind::FastForward,
    );
    let tag = update("refs/tags/v1", zero(), oid('b'), PushUpdateKind::Create);
    let one = record(vec![main.clone(), tag.clone()]);
    let other_order = record(vec![tag, main.clone()]);
    let request = rid("req-push-1");
    assert_eq!(one.digest(&request), other_order.digest(&request));
    assert_ne!(one.digest(&request), one.digest(&rid("req-push-2")));
    // A→B and B→A are different updates; A→B twice differs only by request.
    let back = record(vec![update(
        "refs/heads/main",
        oid('b'),
        oid('a'),
        PushUpdateKind::Force,
    )]);
    let forth = record(vec![main]);
    assert_ne!(forth.digest(&request), back.digest(&request));
    // Length prefixes: moving bytes between fields changes the digest.
    let a = RequestId::parse("ab").expect("id");
    let b = RequestId::parse("a").expect("id");
    assert_ne!(forth.digest(&a), forth.digest(&b));
}

fn json_len(v: &serde_json::Value) -> usize {
    v.to_string().len()
}

#[test]
fn the_json_lists_refs_and_commits_and_says_what_it_left_out() {
    let mut main = update(
        "refs/heads/main",
        oid('a'),
        oid('b'),
        PushUpdateKind::FastForward,
    );
    main.commits = vec![oid('c'); MAX_LISTED_COMMITS];
    main.total_commits = 35;
    let delete = update("refs/heads/old", oid('a'), zero(), PushUpdateKind::Delete);
    let push = record(vec![main, delete]);
    let v = push_json(
        ProjectId::new(3),
        push.repository_id,
        &push.sorted_updates(),
        None,
        EVENT_BUDGET,
    );
    assert_eq!(v["project_id"], 3);
    assert_eq!(v["repository_id"], 7);
    assert_eq!(v["total_refs"], 2);
    assert_eq!(v["refs_truncated"], false);
    assert_eq!(v["total_commits"], 35);
    assert_eq!(v["commits_truncated"], true);
    assert_eq!(v["created"], 0);
    assert_eq!(v["fast_forward"], 1);
    assert_eq!(v["forced"], 0);
    assert_eq!(v["deleted"], 1);
    assert_eq!(v["replayed_from"], serde_json::Value::Null);
    // The delete is listed before the fast-forward.
    let refs = v["refs"].as_array().expect("refs");
    assert_eq!(refs[0]["ref"], "refs/heads/old");
    assert_eq!(refs[0]["kind"], "delete");
    assert_eq!(refs[0]["commits_truncated"], false);
    assert_eq!(refs[1]["ref"], "refs/heads/main");
    assert_eq!(refs[1]["kind"], "fast_forward");
    assert_eq!(refs[1]["commits"].as_array().map(Vec::len), Some(20));
    assert_eq!(refs[1]["commits_truncated"], true);
}

/// Force and delete updates are listed first (each kind in name order), so
/// truncation drops creates and fast-forwards before them; the kind counts
/// cover every update.
#[test]
fn force_and_delete_refs_survive_truncation() {
    let mut updates: Vec<PushedRef> = (0..150)
        .map(|i| {
            update(
                &format!("refs/heads/a{i:03}"),
                zero(),
                oid('a'),
                PushUpdateKind::Create,
            )
        })
        .collect();
    updates.extend((0..40).map(|i| {
        update(
            &format!("refs/heads/f{i:03}"),
            oid('a'),
            oid('b'),
            PushUpdateKind::FastForward,
        )
    }));
    for n in ["refs/heads/z2", "refs/heads/z1"] {
        updates.push(update(n, oid('a'), oid('c'), PushUpdateKind::Force));
    }
    updates.push(update(
        "refs/heads/y",
        oid('a'),
        zero(),
        PushUpdateKind::Delete,
    ));
    let push = record(updates);
    push.check().expect("valid");
    let replay = PushReplay {
        pusher: UserId::new(11),
        credential_id: Some(5),
    };
    for budget in [AUDIT_BUDGET, EVENT_BUDGET] {
        let v = push_json(
            ProjectId::new(3),
            push.repository_id,
            &push.sorted_updates(),
            Some(replay),
            budget,
        );
        assert!(json_len(&v) <= budget);
        assert_eq!(v["total_refs"], 193);
        assert_eq!(v["refs_truncated"], true);
        assert_eq!(v["created"], 150);
        assert_eq!(v["fast_forward"], 40);
        assert_eq!(v["forced"], 2);
        assert_eq!(v["deleted"], 1);
        assert_eq!(
            v["replayed_from"],
            serde_json::json!({ "user_id": 11, "credential_id": 5 })
        );
        let listed: Vec<(&str, &str)> = v["refs"]
            .as_array()
            .expect("refs")
            .iter()
            .map(|r| {
                (
                    r["ref"].as_str().unwrap_or_default(),
                    r["kind"].as_str().unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            listed.get(..4),
            Some(
                &[
                    ("refs/heads/z1", "force"),
                    ("refs/heads/z2", "force"),
                    ("refs/heads/y", "delete"),
                    ("refs/heads/a000", "create"),
                ][..]
            ),
            "{budget}"
        );
        assert!(listed.iter().all(|(_, k)| *k != "fast_forward"), "{budget}");
    }
}

#[test]
fn the_json_stays_within_its_budget_and_flags_truncation() {
    // 1000 refs with long names and SHA-256 ids, 20 commits each.
    let wide = |c: char| ObjectId::parse(&c.to_string().repeat(64)).expect("oid");
    let updates: Vec<PushedRef> = (0..MAX_PUSH_UPDATES)
        .map(|i| PushedRef {
            name: name(&format!("refs/heads/{}-{i:04}", "x".repeat(200))),
            old: wide('a'),
            new: wide('b'),
            kind: PushUpdateKind::Force,
            commits: vec![wide('c'); MAX_LISTED_COMMITS],
            total_commits: 50,
        })
        .collect();
    let push = record(updates);
    push.check().expect("valid");
    let sorted = push.sorted_updates();
    for budget in [AUDIT_BUDGET, EVENT_BUDGET] {
        let v = push_json(ProjectId::new(3), push.repository_id, &sorted, None, budget);
        assert!(json_len(&v) <= budget, "{} > {budget}", json_len(&v));
        assert_eq!(v["total_refs"], 1000);
        assert_eq!(v["refs_truncated"], true);
        assert_eq!(v["total_commits"], 50_000);
        assert_eq!(v["commits_truncated"], true);
        let listed = v["refs"].as_array().expect("refs").len();
        assert!((1..=MAX_LISTED_REFS).contains(&listed), "{listed}");
    }
    // Short names: the event lists 100 refs, the audit details fewer.
    let short: Vec<PushedRef> = (0..150)
        .map(|i| {
            update(
                &format!("refs/heads/b{i:03}"),
                zero(),
                oid('a'),
                PushUpdateKind::Create,
            )
        })
        .collect();
    let push = record(short);
    let event = push_json(
        ProjectId::new(3),
        push.repository_id,
        &push.sorted_updates(),
        None,
        EVENT_BUDGET,
    );
    assert_eq!(
        event["refs"].as_array().map(Vec::len),
        Some(MAX_LISTED_REFS)
    );
    assert_eq!(event["refs_truncated"], true);
    assert_eq!(event["total_refs"], 150);
    // Every listed ref has its commit, the 50 left out have theirs: truncated.
    assert_eq!(event["commits_truncated"], true);
    let audit = push_json(
        ProjectId::new(3),
        push.repository_id,
        &push.sorted_updates(),
        None,
        AUDIT_BUDGET,
    );
    assert!(json_len(&audit) <= AUDIT_BUDGET);
    assert_eq!(audit["refs_truncated"], true);
    // One ref whose commits do not fit is listed without them.
    let mut huge = PushedRef {
        name: name(&format!("refs/heads/{}", "y".repeat(1000))),
        old: wide('a'),
        new: wide('b'),
        kind: PushUpdateKind::FastForward,
        commits: vec![wide('c'); MAX_LISTED_COMMITS],
        total_commits: 20,
    };
    let v = push_json(
        ProjectId::new(3),
        RepoId::new(7),
        std::slice::from_ref(&huge),
        None,
        2048,
    );
    assert_eq!(v["refs"][0]["commits"].as_array().map(Vec::len), Some(0));
    assert_eq!(v["refs"][0]["commits_truncated"], true);
    assert_eq!(v["commits_truncated"], true);
    assert_eq!(v["refs_truncated"], false);
    huge.commits.clear();
    huge.total_commits = 0;
    let v = push_json(ProjectId::new(3), RepoId::new(7), &[huge], None, 2048);
    assert_eq!(v["commits_truncated"], false);
}

#[test]
fn a_push_that_cannot_be_recorded_is_push_record_failed() {
    let push = record(vec![update(
        "refs/heads/main",
        oid('a'),
        oid('b'),
        PushUpdateKind::FastForward,
    )]);
    let cause = ForgeError::new(ErrorCode::DbUnavailable).with_context("reason", "timeout");
    let err = push.failed(cause, 3);
    assert_eq!(err.code(), ErrorCode::PushRecordFailed);
    assert_eq!(err.code().id(), "FG-GIT-0012");
    assert_eq!(err.code().severity(), forge_errors::Severity::Alert);
    for (key, value) in [
        ("reason", "record_failed"),
        ("repo_id", "7"),
        ("refs", "1"),
        ("attempts", "3"),
        ("cause_code", "FG-DB-0001"),
    ] {
        assert_eq!(reason(&err, key), value, "{key}: {err}");
    }
    assert!(std::error::Error::source(&err).is_some());
    assert!(!err.to_string().contains("refs/heads/main"), "{err}");
}

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn pusher() -> Pusher {
    Pusher {
        request_id: rid("req-push-log"),
        user: UserId::new(11),
        credential_id: Some(5),
        source_ip: None,
        user_agent: None,
    }
}

/// What `PushApplied::log` writes with the production JSON format.
fn logged_line(applied: &PushApplied) -> String {
    let buf = Buf::default();
    let writer = buf.clone(); // clone: the subscriber writes, the test reads
    let subscriber = tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || applied.log().expect("log"));
    let bytes = buf.0.lock().expect("lock").clone();
    String::from_utf8(bytes).expect("utf-8")
}

#[test]
fn the_push_applied_line_round_trips_through_the_log() {
    let mut main = update(
        "refs/heads/main",
        oid('a'),
        oid('b'),
        PushUpdateKind::FastForward,
    );
    main.commits = vec![oid('b'), oid('c')];
    main.total_commits = 9;
    let push = record(vec![
        main,
        update("refs/tags/v1", zero(), oid('b'), PushUpdateKind::Create),
    ]);
    let applied = PushApplied::new(&pusher(), ProjectId::new(3), &push);
    let line = logged_line(&applied);
    assert_eq!(line.lines().count(), 1, "{line}");
    let event: serde_json::Value = serde_json::from_str(&line).expect("json");
    assert_eq!(event["event"], PUSH_APPLIED_EVENT);
    assert_eq!(event["request_id"], "req-push-log");
    assert_eq!(event["repo_id"], 7);
    assert_eq!(event["refs"], 2);
    let parsed = PushApplied::from_log_line(&line)
        .expect("readable")
        .expect("a push line");
    assert_eq!(parsed, applied);
    let (who, project, back) = parsed.to_parts().expect("parts");
    assert_eq!(who, pusher());
    assert_eq!(project, ProjectId::new(3));
    assert_eq!(back, push);
    // The same event under `fields` (the non-flattened layout) reads too.
    let nested = serde_json::json!({ "fields": event }).to_string();
    assert_eq!(
        PushApplied::from_log_line(&nested).expect("readable"),
        Some(applied)
    );
}

#[test]
fn other_lines_are_skipped_and_a_broken_push_line_is_refused() {
    for other in [
        "",
        "not json",
        "2026-11-01T00:00:00Z INFO forge.git.push_applied",
        r#"{"event":"forge.http.request","request_id":"r"}"#,
        r#"[1,2]"#,
    ] {
        assert_eq!(
            PushApplied::from_log_line(other).expect("skipped"),
            None,
            "{other}"
        );
    }
    let push = record(vec![update(
        "refs/heads/main",
        oid('a'),
        oid('b'),
        PushUpdateKind::FastForward,
    )]);
    let applied = PushApplied::new(&pusher(), ProjectId::new(3), &push);
    let good = applied.to_json().expect("json");
    let line = |request: &str, push: &str| {
        serde_json::json!({ "event": PUSH_APPLIED_EVENT, "request_id": request, "push": push })
            .to_string()
    };
    let refused = |l: String| {
        let err = PushApplied::from_log_line(&l).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::PushRecordFailed);
        assert_eq!(reason(&err, "reason"), "push_log_invalid");
        reason(&err, "field")
    };
    assert_eq!(
        refused(serde_json::json!({ "event": PUSH_APPLIED_EVENT }).to_string()),
        "push"
    );
    assert_eq!(refused(line("req-push-log", "{")), "push_json");
    assert_eq!(refused(line("req-other", &good)), "request_id");
    let unknown = good.replacen("{", r#"{"extra":1,"#, 1);
    assert_eq!(refused(line("req-push-log", &unknown)), "push_json");
    let v2 = good.replacen(r#""v":1"#, r#""v":2"#, 1);
    assert_eq!(refused(line("req-push-log", &v2)), "v");
    // Readable, but not a record git can have applied.
    let mut bad = applied.clone();
    bad.updates[0].kind = "create".to_owned();
    let err = bad.to_parts().expect_err("inconsistent");
    assert_eq!(reason(&err, "rule"), "kind_mismatch");
    let mut bad = applied.clone();
    bad.updates[0].name = "main".to_owned();
    assert_eq!(reason(&bad.to_parts().expect_err("ref"), "field"), "ref");
    let mut bad = applied;
    bad.user_id = 0;
    assert_eq!(
        reason(&bad.to_parts().expect_err("user"), "field"),
        "user_id"
    );
}

#[test]
fn protection_levels_are_every_matching_rule_in_order() {
    let rule = |p: &str, role| ProtectionRule {
        pattern: RefPattern::parse(p).expect("pattern"),
        push_min_role: role,
    };
    let rules = ProtectionRules::new(vec![
        rule("refs/heads/*", Role::Maintainer),
        rule("refs/heads/main", Role::Developer),
        rule("refs/heads/MA*", Role::Owner),
    ]);
    assert_eq!(rules.len(), 3);
    assert_eq!(
        rules.levels(&name("refs/heads/MAIN")),
        [Role::Maintainer, Role::Developer, Role::Owner]
    );
    assert_eq!(rules.levels(&name("refs/heads/dev")), [Role::Maintainer]);
    assert!(rules.levels(&name("refs/tags/v1")).is_empty());
    assert!(ProtectionRules::default().is_empty());
}
