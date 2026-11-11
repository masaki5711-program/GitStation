//! The two statements of `activity` against a migrated database (plan 15
//! MR 8): the membership walk, the keyset by `seq` across events of one
//! transaction (one timestamp), the type filter, and the scalars read only
//! when they have the expected shape.

use forge_core::{RequestId, Role};

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::groups::{self, NewGroup};
use crate::projects::{self, NewProject};
use crate::sequencer::{self, AuditSequencing, BatchSize};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

async fn person(t: &FreshDb, name: &str) -> UserId {
    users::create(
        &t.db,
        &system(&format!("req-user-{name}")),
        &NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("user")
    .user_id
}

/// A project `<group>/web` in a new top-level group owned by `owner`.
async fn project(t: &FreshDb, group: &str, owner: UserId) -> (ProjectId, forge_core::GroupId) {
    let g = groups::create(
        &t.db,
        &system(&format!("req-{group}")),
        &NewGroup {
            segment: group.into(),
            parent: None,
            display_name: group.into(),
            description: String::new(),
            owner,
        },
    )
    .await
    .expect("group");
    let p = projects::create(
        &t.db,
        &system(&format!("req-{group}-web")),
        &NewProject::new("web", g.namespace_id, "Web", owner),
    )
    .await
    .expect("project")
    .project_id;
    (p, g.group_id)
}

/// The one merge request `iid` of `project`, titled `Change <iid>`; its id.
async fn mr(t: &FreshDb, project: ProjectId, iid: i64, author: UserId) -> i64 {
    let p = project.get();
    t.commit_audited(
        ("project", p),
        &[
            format!(
                "INSERT INTO forge.merge_request_counters (project_id, last_iid) \
                 VALUES ({p}, {iid})"
            ),
            format!(
                "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, \
                 source_branch, target_branch, head_sha) VALUES ({p}, {iid}, 'Change {iid}', \
                 {}, 'refs/heads/b{iid}', 'refs/heads/main', '{}')",
                author.get(),
                "a".repeat(40)
            ),
        ],
    )
    .await;
    let rows = t
        .json_rows(&format!(
            "SELECT id FROM forge.merge_requests WHERE project_id = {p} AND iid = {iid}"
        ))
        .await;
    rows[0]["id"].as_i64().expect("id")
}

/// `events` (type, payload) of `project` by `actor`, written in one
/// transaction (one `occurred_at`) with its audit row, then numbered.
async fn events(t: &FreshDb, project: ProjectId, actor: UserId, events: &[(&str, String)]) {
    let p = project.get();
    let values: Vec<String> = events
        .iter()
        .map(|(kind, payload)| {
            format!(
                "(gen_random_uuid(), '{kind}', 1, 'project', {p}, {p}, 'user', {a}, 'r-act', \
                 '{payload}'::jsonb)",
                a = actor.get()
            )
        })
        .collect();
    t.exec_superuser(&format!(
        "BEGIN; \
         INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, resource_id, \
           request_id) VALUES ('system', 'probe.activity', 'succeeded', 'project', {p}, 'r-act'); \
         INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
           aggregate_id, project_id, actor_kind, actor_user_id, request_id, payload) VALUES {}; \
         COMMIT",
        values.join(", ")
    ))
    .await;
    sequencer::run_once(&t.db, BatchSize::MAX, AuditSequencing::Skip)
        .await
        .expect("sequencer");
}

#[tokio::test]
async fn involved_projects_follow_the_membership_walk() {
    let t = FreshDb::migrated().await;
    let olga = person(&t, "olga").await;
    let dave = person(&t, "dave").await;
    let rita = person(&t, "rita").await;
    let hugo = person(&t, "hugo").await;
    let stranger = person(&t, "stranger").await;
    let (web, acme) = project(&t, "acme", olga).await;
    let (secret, _) = project(&t, "secret", hugo).await;
    // dave through the group, rita directly on hugo's project.
    groups::add_member(&t.db, &system("req-gm"), acme, dave, Role::Reporter)
        .await
        .expect("group member");
    projects::add_member(&t.db, &system("req-pm"), secret, rita, Role::Reporter)
        .await
        .expect("project member");
    let ids = |i: InvolvedProjects| {
        assert!(!i.truncated);
        i.ids
    };
    assert_eq!(
        ids(involved_projects(&t.db, dave).await.expect("dave")),
        vec![web]
    );
    assert_eq!(
        ids(involved_projects(&t.db, rita).await.expect("rita")),
        vec![secret]
    );
    assert_eq!(
        ids(involved_projects(&t.db, hugo).await.expect("hugo")),
        vec![secret]
    );
    assert_eq!(
        ids(involved_projects(&t.db, stranger).await.expect("stranger")),
        Vec::<ProjectId>::new()
    );
}

#[tokio::test]
async fn pages_chain_by_seq_within_one_timestamp_and_read_only_shaped_scalars() {
    let t = FreshDb::migrated().await;
    let olga = person(&t, "olga").await;
    let (web, _) = project(&t, "acme", olga).await;
    let (other, _) = project(&t, "other", olga).await;
    let mine = mr(&t, web, 1, olga).await;
    let foreign = mr(&t, other, 7, olga).await;
    events(
        &t,
        web,
        olga,
        &[
            (
                "merge_request.opened",
                format!(r#"{{"merge_request_id": {mine}, "iid": 1, "author_id": 1}}"#),
            ),
            // Malformed: neither the iid nor the title is read.
            (
                "merge_request.closed",
                r#"{"merge_request_id": "1", "iid": "1"}"#.to_owned(),
            ),
            (
                "repository.pushed",
                r#"{"total_refs": 2, "refs": []}"#.to_owned(),
            ),
            ("member.added", r#"{"user_id": 9}"#.to_owned()),
            // Another project's merge request: no title from it.
            (
                "merge_request.merged",
                format!(r#"{{"merge_request_id": {foreign}, "iid": 7}}"#),
            ),
        ],
    )
    .await;
    let types = [
        "merge_request.opened",
        "merge_request.closed",
        "merge_request.merged",
        "repository.pushed",
    ];
    let mut seen: Vec<ActivityRow> = Vec::new();
    let mut before = None;
    for _ in 0..4 {
        let page = activity_page(&t.db, &[web, other], &types, before, 2)
            .await
            .expect("page");
        let full = page.len() == 2;
        before = page.last().map(|r| r.seq);
        seen.extend(page);
        if !full {
            break;
        }
    }
    let kinds: Vec<&str> = seen.iter().map(|r| r.event_type.as_str()).collect();
    assert_eq!(
        kinds,
        vec![
            "merge_request.merged",
            "repository.pushed",
            "merge_request.closed",
            "merge_request.opened",
        ]
    );
    // Strictly newest first, every row once, all of one timestamp.
    assert!(seen.windows(2).all(|w| w[0].seq > w[1].seq), "{seen:?}");
    assert!(seen.iter().all(|r| r.occurred_at == seen[0].occurred_at));
    let merged = &seen[0];
    assert_eq!((merged.iid, merged.title.as_deref()), (Some(7), None));
    let pushed = &seen[1];
    assert_eq!((pushed.total_refs, pushed.iid), (Some(2), None));
    let closed = &seen[2];
    assert_eq!((closed.iid, closed.title.as_deref()), (None, None));
    let opened = &seen[3];
    assert_eq!(
        (opened.iid, opened.title.as_deref()),
        (Some(1), Some("Change 1"))
    );
    assert_eq!(opened.actor_kind, "user");
    assert_eq!(opened.actor_username.as_deref(), Some("olga"));
    assert_eq!(opened.project_id, web.get());
    // `before` is exclusive: the oldest row's seq gives nothing older.
    let after_last = activity_page(&t.db, &[web], &types, Some(opened.seq), 10)
        .await
        .expect("page");
    assert!(after_last.is_empty(), "{after_last:?}");
    // No project or no type reads nothing.
    assert!(activity_page(&t.db, &[], &types, None, 10)
        .await
        .expect("empty")
        .is_empty());
    assert!(activity_page(&t.db, &[web], &[], None, 10)
        .await
        .expect("empty")
        .is_empty());
}

/// The batch access read of the involved projects (review MEDIUM "N+1"):
/// one statement, newest first, each with the viewer's role as
/// `resolve::load_project` reads it; a missing or repeated id is dropped.
#[tokio::test]
async fn project_records_read_every_access_row_in_one_statement() {
    let t = FreshDb::migrated().await;
    let olga = person(&t, "olga").await;
    let dave = person(&t, "dave").await;
    let (web, acme) = project(&t, "acme", olga).await;
    let (other, _) = project(&t, "other", olga).await;
    groups::add_member(&t.db, &system("req-gm"), acme, dave, Role::Guest)
        .await
        .expect("group member");
    let records =
        crate::lists::project_records(&t.db, &[web, other, web, ProjectId::new(999_999)], dave)
            .await
            .expect("records");
    let seen: Vec<(ProjectId, Option<Role>, String)> = records
        .iter()
        .map(|r| (r.id, r.access.role, r.path.full_path()))
        .collect();
    assert_eq!(
        seen,
        vec![
            (other, None, "other/web".to_owned()),
            (web, Some(Role::Guest), "acme/web".to_owned()),
        ]
    );
    for (record, id) in records.iter().zip([other, web]) {
        let one = crate::resolve::load_project(&t.db, id, Some(dave))
            .await
            .expect("one")
            .expect("exists");
        assert_eq!(&one, record);
    }
    assert!(crate::lists::project_records(&t.db, &[], dave)
        .await
        .expect("empty")
        .is_empty());
}
