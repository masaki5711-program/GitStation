//! The three statements of `user_lists` executed against a migrated
//! database (plan 15 MR 6 review): each scope's candidates and facts, the
//! membership walk through a group, an approval of the current head, the
//! keyset, and the facts computed on the page's rows.

use forge_core::{RequestId, Role};

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::groups::{self, NewGroup};
use crate::projects::{self, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

fn head(n: i64) -> String {
    format!("{n:040x}")
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

/// `(iid, conflict, conflict_predicted, failed_checks)` of each row.
fn seen(rows: &[UserListRow]) -> Vec<(i64, bool, bool, Vec<String>)> {
    rows.iter()
        .map(|r| {
            (
                r.merge_request.iid,
                r.conflict,
                r.conflict_predicted,
                r.failed_checks.clone(),
            )
        })
        .collect()
}

#[tokio::test]
async fn every_scope_statement_runs_and_reads_its_candidates() {
    let t = FreshDb::migrated().await;
    let olga = person(&t, "olga").await;
    let dave = person(&t, "dave").await;
    // dave is a member of the group only: the walk reaches the project.
    let group = groups::create(
        &t.db,
        &system("req-acme"),
        &NewGroup {
            segment: "acme".into(),
            parent: None,
            display_name: "Acme".into(),
            description: String::new(),
            owner: olga,
        },
    )
    .await
    .expect("group");
    groups::add_member(
        &t.db,
        &system("req-member"),
        group.group_id,
        dave,
        Role::Developer,
    )
    .await
    .expect("member");
    let p = projects::create(
        &t.db,
        &system("req-web"),
        &NewProject::new("web", group.namespace_id, "Web", olga),
    )
    .await
    .expect("project")
    .project_id
    .get();
    // 1, 2: dave's (2 conflicts and has a failed run); 3, 4, 5: olga's (4
    // approved by dave on its head, 5 a draft); 6: olga's, closed.
    let mut statements = vec![format!(
        "INSERT INTO forge.merge_request_counters (project_id, last_iid) VALUES ({p}, 6)"
    )];
    for (iid, author, draft, state) in [
        (1, dave, false, "open"),
        (2, dave, false, "open"),
        (3, olga, false, "open"),
        (4, olga, false, "open"),
        (5, olga, true, "open"),
        (6, olga, false, "closed"),
    ] {
        let closed = if state == "closed" {
            format!("now(), {}", olga.get())
        } else {
            "NULL, NULL".to_owned()
        };
        statements.push(format!(
            "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, \
             source_branch, target_branch, draft, head_sha, state, closed_at, closed_by) \
             VALUES ({p}, {iid}, 'Change {iid}', {}, 'refs/heads/b{iid}', 'refs/heads/main', \
             {draft}, '{}', '{state}', {closed})",
            author.get(),
            head(iid)
        ));
    }
    statements.push(format!(
        "INSERT INTO forge.merge_request_approvals (merge_request_id, project_id, \
         approver_id, head_sha, target_branch) SELECT id, project_id, {}, head_sha, \
         target_branch FROM forge.merge_requests WHERE project_id = {p} AND iid = 4",
        dave.get()
    ));
    statements.push(format!(
        "INSERT INTO forge.merge_request_mergeability (merge_request_id, project_id, status, \
         checked_head_sha, last_decided) SELECT id, project_id, 'cannot_be_merged', head_sha, \
         'cannot_be_merged' FROM forge.merge_requests WHERE project_id = {p} AND iid = 2"
    ));
    statements.push(format!(
        "INSERT INTO forge.check_runs (project_id, head_sha, name, status, conclusion, \
         completed_at, reported_by, request_id) VALUES ({p}, '{}', 'ci/build', 'completed', \
         'failure', now(), {}, 'req-check')",
        head(2),
        olga.get()
    ));
    t.commit_audited(("project", p), &statements).await;

    let read = |user, scope, before, limit| user_merge_requests(&t.db, user, scope, before, limit);

    let authored = read(dave, UserListScope::Authored, None, 10)
        .await
        .expect("authored");
    assert_eq!(
        seen(&authored),
        vec![
            (2, true, false, vec!["ci/build".to_owned()]),
            (1, false, false, Vec::new()),
        ]
    );
    // The keyset: below the newest one's id.
    let older = read(
        dave,
        UserListScope::Authored,
        Some(authored[0].merge_request.id.get()),
        10,
    )
    .await
    .expect("older");
    assert_eq!(seen(&older), vec![(1, false, false, Vec::new())]);

    let approvable = read(dave, UserListScope::Approvable, None, 10)
        .await
        .expect("approvable");
    assert_eq!(seen(&approvable), vec![(3, false, false, Vec::new())]);
    // olga (the group's Owner) is offered dave's, newest first, one page.
    let olgas = read(olga, UserListScope::Approvable, None, 1)
        .await
        .expect("olga approvable");
    assert_eq!(
        seen(&olgas),
        vec![(2, true, false, vec!["ci/build".to_owned()])]
    );

    let attention = read(dave, UserListScope::Attention, None, 10)
        .await
        .expect("attention");
    assert_eq!(
        seen(&attention),
        vec![(2, true, false, vec!["ci/build".to_owned()])]
    );
    let none = read(olga, UserListScope::Attention, None, 10)
        .await
        .expect("olga attention");
    assert!(none.is_empty());
}
