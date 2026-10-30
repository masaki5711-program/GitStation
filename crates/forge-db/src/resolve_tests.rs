//! The namespace resolver and the project access loader (M1 phase 8 MR3):
//! live / moved / deleted / absent, case folding, the viewer's role
//! (direct, inherited, none, anonymous) as the role loaders compute it,
//! raw visibility and pending deletion, and one identical statement per
//! call whatever the outcome. Fixtures go through the user, group and
//! project commands; moves and tombstones (D5 has no command yet) are
//! written as forge_app with their audit rows.

use forge_core::RequestId;
use forge_core::{LookupPath, Role, Visibility};
use forge_errors::ErrorCode;

use super::*;
use crate::audited_tests::{audit, commit};
use crate::change::{Actor, ChangeContext};
use crate::groups::{self, CreatedGroup, NewGroup};
use crate::jobs::runner_stop_tests::Captured;
use crate::projects::{self, CreatedProject, NewProject};
use crate::roles::{load_group_roles, load_project_roles};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

async fn user(t: &FreshDb, name: &str) -> users::Created {
    users::create(
        &t.db,
        &ctx(&format!("req-user-{name}")),
        &NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("user")
}

async fn group(
    t: &FreshDb,
    segment: &str,
    parent: Option<&CreatedGroup>,
    owner: UserId,
) -> CreatedGroup {
    let new = NewGroup {
        segment: segment.to_owned(),
        parent: parent.map(|p| p.namespace_id),
        display_name: format!("Group {segment}"),
        description: String::new(),
        owner,
    };
    groups::create(&t.db, &ctx(&format!("req-group-{segment}")), &new)
        .await
        .unwrap_or_else(|e| panic!("{segment}: {e}"))
}

async fn project(
    t: &FreshDb,
    segment: &str,
    parent: NamespaceId,
    creator: UserId,
    visibility: Visibility,
) -> CreatedProject {
    let new = NewProject {
        visibility,
        ..NewProject::new(segment, parent, &format!("Project {segment}"), creator)
    };
    projects::create(&t.db, &ctx(&format!("req-project-{segment}")), &new)
        .await
        .unwrap_or_else(|e| panic!("{segment}: {e}"))
}

async fn add_project_member(t: &FreshDb, p: &CreatedProject, user: UserId, role: Role) {
    projects::add_member(
        &t.db,
        &ctx(&format!("req-member-{}", user.get())),
        p.project_id,
        user,
        role,
    )
    .await
    .expect("add project member");
}

/// The folded lookup of a request path's namespace part.
fn path(raw: &str) -> LookupPath {
    let segments: Vec<&str> = raw.split('/').collect();
    LookupPath::fold(&segments).expect("a valid lookup path")
}

/// Rewrite a namespace row's segment and full path (as D5's rename will),
/// with the audit row of its owner.
fn rewrite(
    kind: &str,
    owner: i64,
    namespace: NamespaceId,
    segment: &str,
    full: &str,
) -> [String; 2] {
    [
        audit(kind, owner, "succeeded"),
        format!(
            "UPDATE forge.namespaces SET segment = '{segment}', full_path = '{full}' \
             WHERE id = {}",
            namespace.get()
        ),
    ]
}

/// A tombstone at `old_path` (`moved` to `target`, or `deleted`), with the
/// audit row of its cause.
fn tombstone(old_path: &str, target: Option<NamespaceId>, cause: (&str, i64)) -> [String; 2] {
    let (kind, target) = match target {
        Some(t) => ("moved", t.get().to_string()),
        None => ("deleted", "NULL".to_owned()),
    };
    [
        audit(cause.0, cause.1, "succeeded"),
        format!(
            "INSERT INTO forge.namespace_tombstones (old_path, kind, namespace_id, cause_type, \
             cause_id) VALUES ('{old_path}', '{kind}', {target}, '{}', {})",
            cause.0, cause.1
        ),
    ]
}

async fn apply(t: &FreshDb, parts: &[[String; 2]]) {
    let stmts: Vec<String> = parts.iter().flatten().cloned().collect();
    commit(t, &stmts).await.expect("fixture change");
}

async fn resolve_ok(t: &FreshDb, raw: &str, viewer: Option<UserId>) -> Resolution {
    resolve(&t.db, &path(raw), viewer)
        .await
        .unwrap_or_else(|e| panic!("{raw}: {e}"))
}

fn live(resolution: Resolution) -> Resolved {
    let Resolution::Live(r) = resolution else {
        panic!("expected a live namespace, got {resolution:?}");
    };
    r
}

/// The parts of a `Moved` answer.
fn moved(resolution: Resolution) -> (Resolved, TombstoneId, NamespacePath) {
    let Resolution::Moved {
        resolved,
        tombstone,
        current,
    } = resolution
    else {
        panic!("expected moved, got {resolution:?}");
    };
    (resolved, tombstone, current)
}

fn project_target(resolved: &Resolved) -> (ProjectId, ProjectAccess) {
    let ResolvedTarget::Project { id, access } = resolved.target else {
        panic!("expected a project, got {resolved:?}");
    };
    (id, access)
}

fn group_target(resolved: &Resolved) -> (GroupId, Option<Role>) {
    let ResolvedTarget::Group { id, viewer_role } = resolved.target else {
        panic!("expected a group, got {resolved:?}");
    };
    (id, viewer_role)
}

#[tokio::test]
async fn live_user_group_and_project_resolve_to_their_targets() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let api = group(&t, "api", Some(&acme), alice.user_id).await;
    let web = project(
        &t,
        "web",
        api.namespace_id,
        alice.user_id,
        Visibility::Private,
    )
    .await;

    assert_eq!(
        live(resolve_ok(&t, "alice", Some(alice.user_id)).await),
        Resolved {
            namespace_id: alice.namespace_id,
            target: ResolvedTarget::User(alice.user_id),
        }
    );
    assert_eq!(
        live(resolve_ok(&t, "acme/api", Some(alice.user_id)).await),
        Resolved {
            namespace_id: api.namespace_id,
            target: ResolvedTarget::Group {
                id: api.group_id,
                viewer_role: Some(Role::Owner),
            },
        }
    );
    assert_eq!(
        live(resolve_ok(&t, "acme/api/web", Some(alice.user_id)).await),
        Resolved {
            namespace_id: web.namespace_id,
            target: ResolvedTarget::Project {
                id: web.project_id,
                access: ProjectAccess {
                    visibility: Visibility::Private,
                    role: Some(Role::Owner),
                    pending_deletion: false,
                },
            },
        }
    );
    t.drop_now().await;
}

#[tokio::test]
async fn an_absent_path_and_a_path_below_a_project_are_absent() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    project(
        &t,
        "web",
        alice.namespace_id,
        alice.user_id,
        Visibility::Internal,
    )
    .await;
    for raw in ["nobody", "alice/nothing", "alice/web/deeper", "ali"] {
        assert_eq!(
            resolve_ok(&t, raw, Some(alice.user_id)).await,
            Resolution::Absent,
            "{raw}"
        );
        assert_eq!(resolve_ok(&t, raw, None).await, Resolution::Absent, "{raw}");
    }
    t.drop_now().await;
}

#[tokio::test]
async fn a_lookup_path_is_case_folded_before_the_exact_match() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let web = project(
        &t,
        "web",
        alice.namespace_id,
        alice.user_id,
        Visibility::Internal,
    )
    .await;
    let resolved = live(resolve_ok(&t, "Alice/WEB", None).await);
    assert_eq!(resolved.namespace_id, web.namespace_id);
    t.drop_now().await;
}

#[tokio::test]
async fn the_viewer_role_is_direct_inherited_none_or_anonymous() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let carol = user(&t, "carol").await;
    let dave = user(&t, "dave").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let api = group(&t, "api", Some(&acme), alice.user_id).await;
    groups::add_member(
        &t.db,
        &ctx("req-carol"),
        acme.group_id,
        carol.user_id,
        Role::Reporter,
    )
    .await
    .expect("carol");
    let web = project(
        &t,
        "web",
        api.namespace_id,
        alice.user_id,
        Visibility::Private,
    )
    .await;
    add_project_member(&t, &web, bob.user_id, Role::Developer).await;

    let cases = [
        // Owner of the top group: inherited through two levels.
        (Some(alice.user_id), Some(Role::Owner)),
        // A direct project member only.
        (Some(bob.user_id), Some(Role::Developer)),
        // Reporter of the top group: inherited.
        (Some(carol.user_id), Some(Role::Reporter)),
        // No membership anywhere.
        (Some(dave.user_id), None),
        // Anonymous: the same statement with a NULL viewer.
        (None, None),
    ];
    for (viewer, want) in cases {
        let (id, access) = project_target(&live(resolve_ok(&t, "acme/api/web", viewer).await));
        assert_eq!(id, web.project_id);
        assert_eq!(access.role, want, "project role of {viewer:?}");
        let by_id = project_access(&t.db, web.project_id, viewer)
            .await
            .expect("project_access")
            .expect("present");
        assert_eq!(by_id, access, "the two loaders agree for {viewer:?}");
        // The group below the top: inherited roles only.
        let (id, viewer_role) = group_target(&live(resolve_ok(&t, "acme/api", viewer).await));
        assert_eq!(id, api.group_id);
        let want_group = match viewer {
            Some(v) if v == bob.user_id => None,
            _ => want,
        };
        assert_eq!(viewer_role, want_group, "group role of {viewer:?}");
    }
    t.drop_now().await;
}

/// The resolver's role is `load_project_roles` / `load_group_roles`'
/// `native` for the same fixtures (the one role computation).
#[tokio::test]
async fn the_resolved_role_matches_the_role_loaders() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let carol = user(&t, "carol").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let api = group(&t, "api", Some(&acme), bob.user_id).await;
    groups::add_member(
        &t.db,
        &ctx("req-carol"),
        api.group_id,
        carol.user_id,
        Role::Maintainer,
    )
    .await
    .expect("carol");
    // In a group, in Alice's personal namespace (her inherited Owner), and
    // one where Carol also has a lower direct role.
    let in_group = project(
        &t,
        "web",
        api.namespace_id,
        bob.user_id,
        Visibility::Internal,
    )
    .await;
    let personal = project(
        &t,
        "notes",
        alice.namespace_id,
        alice.user_id,
        Visibility::Private,
    )
    .await;
    add_project_member(&t, &in_group, carol.user_id, Role::Guest).await;
    add_project_member(&t, &personal, bob.user_id, Role::Reporter).await;
    let disabled = users::disable(&t.db, &ctx("req-disable"), bob.user_id)
        .await
        .expect("disable bob");
    assert_eq!(disabled, users::DisableOutcome::Disabled);

    let projects = [(&in_group, "acme/api/web"), (&personal, "alice/notes")];
    let group_cases = [(&acme, "acme"), (&api, "acme/api")];
    for viewer in [alice.user_id, bob.user_id, carol.user_id] {
        for (p, raw) in projects {
            let want = load_project_roles(&t.db, p.project_id, viewer)
                .await
                .expect("load_project_roles")
                .native;
            let (_, access) = project_target(&live(resolve_ok(&t, raw, Some(viewer)).await));
            assert_eq!(access.role, want, "{raw} for {viewer:?}");
            let by_id = project_access(&t.db, p.project_id, Some(viewer))
                .await
                .expect("project_access")
                .expect("present");
            assert_eq!(by_id.role, want, "{raw} by id for {viewer:?}");
        }
        for (g, raw) in group_cases {
            let want = load_group_roles(&t.db, g.group_id, viewer)
                .await
                .expect("load_group_roles")
                .native;
            let (_, viewer_role) = group_target(&live(resolve_ok(&t, raw, Some(viewer)).await));
            assert_eq!(viewer_role, want, "{raw} for {viewer:?}");
        }
    }
    // The fixtures exercise more than "no role": a disabled member has none.
    let (_, access) = project_target(&live(
        resolve_ok(&t, "acme/api/web", Some(carol.user_id)).await,
    ));
    assert_eq!(access.role, Some(Role::Maintainer));
    let (_, access) = project_target(&live(
        resolve_ok(&t, "alice/notes", Some(bob.user_id)).await,
    ));
    assert_eq!(access.role, None);
    t.drop_now().await;
}

#[tokio::test]
async fn visibility_and_pending_deletion_are_returned_raw() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let stranger = user(&t, "stranger").await;
    let cases = [
        ("private", Visibility::Private),
        ("internal", Visibility::Internal),
    ];
    for (segment, visibility) in cases {
        let p = project(&t, segment, alice.namespace_id, alice.user_id, visibility).await;
        // Never filtered here: a stranger and an anonymous caller get the
        // same facts (authorize() decides).
        for viewer in [Some(stranger.user_id), None] {
            let (id, access) = project_target(&live(
                resolve_ok(&t, &format!("alice/{segment}"), viewer).await,
            ));
            assert_eq!(id, p.project_id);
            assert_eq!(
                access,
                ProjectAccess {
                    visibility,
                    role: None,
                    pending_deletion: false
                }
            );
        }
    }
    let gone = project(
        &t,
        "gone",
        alice.namespace_id,
        alice.user_id,
        Visibility::Internal,
    )
    .await;
    commit(
        &t,
        &[
            audit("project", gone.project_id.get(), "succeeded"),
            format!(
                "UPDATE forge.projects SET deleted_at = now(), \
                 purge_after = now() + interval '7 days' WHERE id = {}",
                gone.project_id.get()
            ),
        ],
    )
    .await
    .expect("mark deleted");
    let (_, access) = project_target(&live(
        resolve_ok(&t, "alice/gone", Some(alice.user_id)).await,
    ));
    assert_eq!(
        access,
        ProjectAccess {
            visibility: Visibility::Internal,
            role: Some(Role::Owner),
            pending_deletion: true
        }
    );
    assert_eq!(
        project_access(&t.db, gone.project_id, None)
            .await
            .expect("by id"),
        Some(ProjectAccess {
            visibility: Visibility::Internal,
            role: None,
            pending_deletion: true
        })
    );
    assert_eq!(
        project_access(&t.db, ProjectId::new(9_999), Some(alice.user_id))
            .await
            .expect("absent"),
        None
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_renamed_project_resolves_moved_with_its_current_path() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let web = project(
        &t,
        "web",
        alice.namespace_id,
        alice.user_id,
        Visibility::Private,
    )
    .await;
    let pid = web.project_id.get();
    apply(
        &t,
        &[
            rewrite("project", pid, web.namespace_id, "site", "alice/site"),
            tombstone("alice/web", Some(web.namespace_id), ("project", pid)),
        ],
    )
    .await;
    let tomb_id = tombstone_id(&t, "alice/web").await;
    let (resolved, tombstone, current) =
        moved(resolve_ok(&t, "alice/WEB", Some(alice.user_id)).await);
    assert_eq!(tombstone, tomb_id);
    assert_eq!(current.full_path(), "alice/site");
    assert_eq!(resolved.namespace_id, web.namespace_id);
    let (id, access) = project_target(&resolved);
    assert_eq!(id, web.project_id);
    assert_eq!(access.role, Some(Role::Owner));
    // The new path is live.
    assert_eq!(
        live(resolve_ok(&t, "alice/site", None).await).namespace_id,
        web.namespace_id
    );
    t.drop_now().await;
}

async fn tombstone_id(t: &FreshDb, old_path: &str) -> TombstoneId {
    let mut su = t.superuser().await;
    let id: i64 =
        sqlx::query_scalar("SELECT id FROM forge.namespace_tombstones WHERE old_path = $1")
            .bind(old_path)
            .fetch_one(&mut su)
            .await
            .expect("tombstone id");
    TombstoneId::new(id)
}

fn current_of(resolution: Resolution) -> (NamespaceId, String) {
    let (resolved, _, current) = moved(resolution);
    (resolved.namespace_id, current.full_path())
}

#[tokio::test]
async fn every_path_of_a_chain_of_moves_resolves_to_the_current_one() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let web = project(
        &t,
        "web",
        alice.namespace_id,
        alice.user_id,
        Visibility::Private,
    )
    .await;
    let pid = web.project_id.get();
    let ns = web.namespace_id;
    // web -> site -> www: each tombstone names the namespace, not the next
    // path, so no chain of tombstones is followed.
    apply(
        &t,
        &[
            rewrite("project", pid, ns, "site", "alice/site"),
            tombstone("alice/web", Some(ns), ("project", pid)),
        ],
    )
    .await;
    apply(
        &t,
        &[
            rewrite("project", pid, ns, "www", "alice/www"),
            tombstone("alice/site", Some(ns), ("project", pid)),
        ],
    )
    .await;
    for raw in ["alice/web", "alice/site"] {
        assert_eq!(
            current_of(resolve_ok(&t, raw, None).await),
            (ns, "alice/www".to_owned()),
            "{raw}"
        );
    }
    t.drop_now().await;
}

#[tokio::test]
async fn old_paths_below_a_moved_group_resolve_to_the_new_subtree() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let api = group(&t, "api", Some(&acme), alice.user_id).await;
    groups::add_member(
        &t.db,
        &ctx("req-bob"),
        api.group_id,
        bob.user_id,
        Role::Developer,
    )
    .await
    .expect("bob");
    let web = project(
        &t,
        "web",
        api.namespace_id,
        alice.user_id,
        Visibility::Private,
    )
    .await;
    // acme/api -> acme/platform, with one tombstone per node of the subtree.
    apply(
        &t,
        &[
            rewrite(
                "group",
                api.group_id.get(),
                api.namespace_id,
                "platform",
                "acme/platform",
            ),
            rewrite(
                "project",
                web.project_id.get(),
                web.namespace_id,
                "web",
                "acme/platform/web",
            ),
            tombstone(
                "acme/api",
                Some(api.namespace_id),
                ("group", api.group_id.get()),
            ),
            tombstone(
                "acme/api/web",
                Some(web.namespace_id),
                ("group", api.group_id.get()),
            ),
        ],
    )
    .await;
    assert_eq!(
        current_of(resolve_ok(&t, "acme/api", None).await),
        (api.namespace_id, "acme/platform".to_owned())
    );
    let (resolved, _, current) = moved(resolve_ok(&t, "acme/api/web", Some(bob.user_id)).await);
    assert_eq!(current.full_path(), "acme/platform/web");
    let (_, access) = project_target(&resolved);
    // Inherited from the moved group.
    assert_eq!(access.role, Some(Role::Developer));
    // Exact matches only: a path created below the old one after the move
    // never existed, so it is absent (no prefix match through a tombstone).
    assert_eq!(
        resolve_ok(&t, "acme/api/other", None).await,
        Resolution::Absent
    );
    t.drop_now().await;
}

#[tokio::test]
async fn deleted_tombstones_and_moved_ones_without_a_target_are_deleted() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let uid = alice.user_id.get();
    apply(
        &t,
        &[
            tombstone("alice/old", None, ("user", uid)),
            // A moved tombstone whose target was purged (namespace_id SET
            // NULL by the foreign key).
            [
                audit("user", uid, "succeeded"),
                format!(
                    "INSERT INTO forge.namespace_tombstones (old_path, kind, cause_type, \
                     cause_id) VALUES ('alice/purged', 'moved', 'user', {uid})"
                ),
            ],
        ],
    )
    .await;
    for raw in ["alice/old", "alice/purged"] {
        let want = tombstone_id(&t, raw).await;
        for viewer in [Some(alice.user_id), None] {
            assert_eq!(
                resolve_ok(&t, raw, viewer).await,
                Resolution::Deleted { tombstone: want },
                "{raw}"
            );
        }
    }
    t.drop_now().await;
}

/// The current path is rebuilt from the target's segments and confirmed by
/// an exact match on the live paths: when the stored full path disagrees
/// (a subtree rewrite that missed a row), the old path never redirects and
/// the corruption is a registered alert, not a silent `Deleted` (only a
/// purged target is `Deleted`).
#[tokio::test]
async fn a_moved_tombstone_whose_rebuilt_path_is_not_live_is_namespace_data_invalid() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let api = group(&t, "api", Some(&acme), alice.user_id).await;
    let web = project(
        &t,
        "web",
        api.namespace_id,
        alice.user_id,
        Visibility::Private,
    )
    .await;
    // The group is renamed, but the project's stored full path disagrees
    // with its segments (`www` vs `web`): the rebuilt `acme/platform/web`
    // matches no live path.
    apply(
        &t,
        &[
            rewrite(
                "group",
                api.group_id.get(),
                api.namespace_id,
                "platform",
                "acme/platform",
            ),
            rewrite(
                "project",
                web.project_id.get(),
                web.namespace_id,
                "web",
                "acme/platform/www",
            ),
            tombstone(
                "acme/api/web",
                Some(web.namespace_id),
                ("group", api.group_id.get()),
            ),
        ],
    )
    .await;
    let tomb = tombstone_id(&t, "acme/api/web").await;
    for viewer in [Some(alice.user_id), None] {
        let err = resolve(&t.db, &path("acme/api/web"), viewer)
            .await
            .expect_err("never a silent Deleted while the target exists");
        assert_reason(
            &err,
            ErrorCode::NamespaceDataInvalid,
            "moved_path_unconfirmed",
        );
        let shown = err.to_string();
        assert!(
            shown.contains(&format!("namespace_id={}", web.namespace_id.get())),
            "{shown}"
        );
        assert!(
            shown.contains(&format!("tombstone_id={}", tomb.get())),
            "{shown}"
        );
        for p in ["acme/platform", "acme/api/web"] {
            assert!(!shown.contains(p), "no path in the error: {shown}");
        }
    }
    t.drop_now().await;
}

#[tokio::test]
async fn a_live_path_wins_over_a_tombstone_of_the_same_path() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let web = project(
        &t,
        "web",
        alice.namespace_id,
        alice.user_id,
        Visibility::Internal,
    )
    .await;
    // Commands never let both exist (the path lock and the tombstone
    // checks); the resolver still answers the live one first.
    apply(
        &t,
        &[tombstone("alice/web", None, ("user", alice.user_id.get()))],
    )
    .await;
    assert_eq!(
        live(resolve_ok(&t, "alice/web", None).await).namespace_id,
        web.namespace_id
    );
    t.drop_now().await;
}

/// The sqlx statements this thread ran while `capture` was installed
/// (target `sqlx::query`, one event per statement, with its full text).
fn statements(capture: &Captured) -> Vec<String> {
    capture
        .events()
        .into_iter()
        .filter(|e| e["target"] == "sqlx::query")
        .map(|e| {
            // sqlx puts the full text in `db.statement` when it differs
            // from the summary, else only the summary.
            let f = &e["fields"];
            let full = f["db.statement"].as_str().unwrap_or_default().trim();
            if full.is_empty() {
                f["summary"].as_str().unwrap_or_default().to_owned()
            } else {
                full.to_owned()
            }
        })
        .collect()
}

/// ADR-0015 step 6 / the NamespaceResolver contract: every outcome — live
/// user, group, visible and invisible project, moved, deleted, a moved one
/// without its target, absent; signed in or anonymous — runs exactly one
/// statement, and it is the same statement text. Counted from sqlx's own
/// per-statement events, so a second query on any branch turns it red.
#[tokio::test]
async fn every_outcome_runs_the_same_single_statement() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let stranger = user(&t, "stranger").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let web = project(
        &t,
        "web",
        acme.namespace_id,
        alice.user_id,
        Visibility::Private,
    )
    .await;
    let pid = web.project_id.get();
    apply(
        &t,
        &[
            rewrite("project", pid, web.namespace_id, "site", "acme/site"),
            tombstone("acme/web", Some(web.namespace_id), ("project", pid)),
            tombstone("acme/gone", None, ("project", pid)),
        ],
    )
    .await;
    // Warm the pool: connection setup is not part of a lookup.
    resolve_ok(&t, "acme", None).await;
    project_access(&t.db, web.project_id, None)
        .await
        .expect("warm");

    let cases: [(&str, Option<UserId>, &str); 9] = [
        ("alice", Some(alice.user_id), "live user"),
        ("acme", Some(stranger.user_id), "live group"),
        ("acme/site", Some(alice.user_id), "visible project"),
        ("acme/site", Some(stranger.user_id), "invisible project"),
        ("acme/site", None, "anonymous"),
        ("acme/web", Some(stranger.user_id), "moved"),
        ("acme/gone", Some(stranger.user_id), "deleted"),
        ("acme/none", Some(stranger.user_id), "absent"),
        ("nobody/at/all", None, "absent anonymous"),
    ];
    let mut kinds = Vec::new();
    for (raw, viewer, label) in cases {
        let capture = Captured::default();
        let guard = capture.install();
        let answer = resolve(&t.db, &path(raw), viewer).await;
        drop(guard);
        let answer = answer.unwrap_or_else(|e| panic!("{label}: {e}"));
        kinds.push(std::mem::discriminant(&answer));
        assert_eq!(statements(&capture), [RESOLVE_SQL], "{label}");
    }
    // The cases did cover every kind of answer.
    kinds.sort_by_key(|d| format!("{d:?}"));
    kinds.dedup();
    assert_eq!(
        kinds.len(),
        4,
        "live, moved, deleted and absent were all exercised"
    );

    for (project, viewer, label) in [
        (web.project_id, Some(alice.user_id), "visible"),
        (web.project_id, Some(stranger.user_id), "invisible"),
        (web.project_id, None, "anonymous"),
        (ProjectId::new(9_999), Some(stranger.user_id), "absent"),
    ] {
        let capture = Captured::default();
        let guard = capture.install();
        let answer = project_access(&t.db, project, viewer).await;
        drop(guard);
        answer.unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(statements(&capture), [PROJECT_ACCESS_SQL], "{label}");
    }
    t.drop_now().await;
}

/// The trait objects forge-web will hold answer exactly like the functions.
#[tokio::test]
async fn the_db_is_the_resolver_and_the_project_access_loader() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let web = project(
        &t,
        "web",
        alice.namespace_id,
        alice.user_id,
        Visibility::Internal,
    )
    .await;
    let resolver: &dyn forge_core::NamespaceResolver = &t.db;
    let loader: &dyn forge_core::ProjectAccessLoader = &t.db;
    let via_trait = resolver
        .resolve(&path("alice/web"), Some(alice.user_id))
        .await
        .expect("resolve");
    assert_eq!(
        via_trait,
        resolve_ok(&t, "alice/web", Some(alice.user_id)).await
    );
    assert_eq!(
        loader
            .project_access(web.project_id, None)
            .await
            .expect("project_access"),
        project_access(&t.db, web.project_id, None)
            .await
            .expect("direct")
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_moved_target_path_that_does_not_parse_is_namespace_data_invalid() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let api = group(&t, "api", Some(&acme), alice.user_id).await;
    // A segment the CHECK refuses, consistent in full_path so the rebuilt
    // path is confirmed; the constraint and the audit triggers are dropped /
    // off for the tamper only (this database is dropped).
    t.exec_superuser(&format!(
        "ALTER TABLE forge.namespaces DROP CONSTRAINT namespaces_segment; \
         ALTER TABLE forge.namespaces DISABLE TRIGGER USER; \
         UPDATE forge.namespaces SET segment = 'API', full_path = 'acme/API' WHERE id = {ns}; \
         ALTER TABLE forge.namespaces ENABLE TRIGGER USER; \
         ALTER TABLE forge.namespace_tombstones DISABLE TRIGGER USER; \
         INSERT INTO forge.namespace_tombstones (old_path, kind, namespace_id, cause_type, \
           cause_id) VALUES ('acme/old', 'moved', {ns}, 'group', {gid}); \
         ALTER TABLE forge.namespace_tombstones ENABLE TRIGGER USER",
        ns = api.namespace_id.get(),
        gid = api.group_id.get()
    ))
    .await;
    let err = resolve(&t.db, &path("acme/old"), Some(alice.user_id))
        .await
        .expect_err("never a redirect to a path this build cannot read");
    assert_reason(&err, ErrorCode::NamespaceDataInvalid, "stored_path_invalid");
    let shown = err.to_string();
    assert!(shown.contains("rule=charset"), "{shown}");
    assert!(
        shown.contains(&format!("namespace_id={}", api.namespace_id.get())),
        "{shown}"
    );
    assert!(
        !shown.contains("acme/API"),
        "the path is not logged: {shown}"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn an_unknown_stored_visibility_is_namespace_data_invalid() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let web = project(
        &t,
        "web",
        alice.namespace_id,
        alice.user_id,
        Visibility::Private,
    )
    .await;
    t.exec_superuser(&format!(
        "ALTER TABLE forge.projects DROP CONSTRAINT projects_visibility; \
         ALTER TABLE forge.projects DISABLE TRIGGER USER; \
         UPDATE forge.projects SET visibility = 'everyone' WHERE id = {}; \
         ALTER TABLE forge.projects ENABLE TRIGGER USER",
        web.project_id.get()
    ))
    .await;
    let err = resolve(&t.db, &path("alice/web"), None)
        .await
        .expect_err("never guessed");
    assert_reason(&err, ErrorCode::NamespaceDataInvalid, "unknown_visibility");
    assert!(err.to_string().contains("raw=everyone"), "{err}");
    let err = project_access(&t.db, web.project_id, None)
        .await
        .expect_err("never guessed by id either");
    assert_reason(&err, ErrorCode::NamespaceDataInvalid, "unknown_visibility");
    t.drop_now().await;
}

#[tokio::test]
async fn a_cycle_in_the_resolved_chain_is_membership_data_invalid() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let api = group(&t, "api", Some(&acme), alice.user_id).await;
    let core = group(&t, "core", Some(&api), alice.user_id).await;
    // A moved tombstone into the corrupt subtree.
    apply(
        &t,
        &[tombstone(
            "acme/old",
            Some(core.namespace_id),
            ("group", core.group_id.get()),
        )],
    )
    .await;
    t.exec_superuser(&format!(
        "ALTER TABLE forge.namespaces DISABLE TRIGGER USER; \
         UPDATE forge.namespaces SET parent_id = {} WHERE id = {}; \
         ALTER TABLE forge.namespaces ENABLE TRIGGER USER",
        core.namespace_id.get(),
        api.namespace_id.get()
    ))
    .await;
    // Anonymous too: a corrupt chain is never a partial (empty) role.
    // Through the live path and through the moved tombstone alike (a
    // moved target's broken chain is never a silent Deleted).
    for (raw, viewer) in [
        ("acme/api/core", Some(alice.user_id)),
        ("acme/api/core", None),
        ("acme/old", Some(alice.user_id)),
        ("acme/old", None),
    ] {
        let err = resolve(&t.db, &path(raw), viewer)
            .await
            .expect_err("a cycle never resolves");
        assert_reason(
            &err,
            ErrorCode::MembershipDataInvalid,
            "namespace_chain_too_deep",
        );
    }
    t.drop_now().await;
}
