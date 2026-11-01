//! The sign-in's link-or-create step and the operator's link against a
//! migrated database (M2 MR4, ADR-0021 §4): every branch, what each writes
//! (rows and audit rows) and that a refusal writes nothing. Synthetic users
//! only (`example.test`).

use forge_core::ids::UserId;
use forge_core::{RequestId, UserKind};
use forge_errors::{ErrorCode, ForgeError};

use super::*;
use crate::test_support::*;
use crate::users::{self, NewUser};

const ISSUER: &str = "http://127.0.0.1:58080/realms/gitstation-dev";

fn ctx(actor: Actor, request: &str) -> ChangeContext {
    ChangeContext::new(actor, RequestId::parse(request).expect("request id"))
}

fn system(request: &str) -> ChangeContext {
    ctx(Actor::System, request)
}

/// A user with an employee number (synthetic).
async fn person(t: &FreshDb, name: &str, employee: &str, admin: bool, kind: UserKind) -> UserId {
    let mut new = NewUser::human(name, name, &format!("{name}@example.test"));
    new.employee_number = Some(employee.to_owned());
    new.is_admin = admin;
    new.kind = kind;
    users::create(&t.db, &system(&format!("req-user-{name}")), &new)
        .await
        .expect("create user")
        .user_id
}

fn identity<'a>(
    subject: &'a str,
    employee: Option<&'a str>,
    email: Option<&'a str>,
) -> LoginIdentity<'a> {
    LoginIdentity {
        issuer: ISSUER,
        subject,
        employee_number: employee,
        email,
        email_verified: true,
        preferred_username: Some("NewPerson"),
        name: Some("New Person"),
    }
}

const ON: LinkPolicy = LinkPolicy { auto_link: true };

async fn login(
    t: &FreshDb,
    who: &LoginIdentity<'_>,
    policy: LinkPolicy,
) -> (Result<LoginOutcome, ForgeError>, Option<LoginAccount>) {
    let mut seen = None;
    let result = login_decided(
        &t.db,
        &ctx(Actor::Anonymous, "req-login"),
        who,
        policy,
        |a| {
            seen = Some(a);
            Ok(())
        },
    )
    .await;
    (result, seen)
}

async fn scalar(t: &FreshDb, sql: &str) -> i64 {
    let mut su = t.superuser().await;
    sqlx::query_scalar(sql)
        .fetch_one(&mut su)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

async fn identities(t: &FreshDb) -> i64 {
    scalar(t, "SELECT count(*) FROM forge.user_identities").await
}

fn reason(err: &ForgeError) -> String {
    err.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

#[tokio::test]
async fn an_employee_number_with_the_verified_email_links_once_then_finds_the_user() {
    let t = FreshDb::migrated().await;
    let alice = person(&t, "alice", "SYN0001", false, UserKind::Human).await;
    let who = identity("sub-alice", Some("SYN0001"), Some("ALICE@example.test"));
    let (linked, seen) = login(&t, &who, ON).await;
    assert_eq!(
        linked.expect("linked"),
        LoginOutcome::Linked { user: alice }
    );
    assert_eq!(seen, Some(LoginAccount::Active));
    let rows = t
        .json_rows(
            "SELECT action, actor_kind, details FROM forge.audit_log \
             WHERE action = 'user.identity_linked'",
        )
        .await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["actor_kind"], "anonymous");
    assert_eq!(rows[0]["details"]["via"], "employee_number");
    assert_eq!(rows[0]["details"]["issuer"], ISSUER);
    assert!(!rows[0].to_string().contains("sub-alice"), "{rows:?}");

    // Second sign-in: the identity row decides, nothing is written.
    let audits = scalar(&t, "SELECT count(*) FROM forge.audit_log").await;
    let (again, seen) = login(&t, &who, ON).await;
    assert_eq!(
        again.expect("existing"),
        LoginOutcome::Existing { user: alice }
    );
    assert_eq!(seen, Some(LoginAccount::Active));
    assert_eq!(
        scalar(&t, "SELECT count(*) FROM forge.audit_log").await,
        audits
    );
    record_login(&t.db, ISSUER, "sub-alice")
        .await
        .expect("last login");
    t.drop_now().await;
}

#[tokio::test]
async fn a_first_sign_in_creates_a_human_with_the_identity_in_one_change() {
    let t = FreshDb::migrated().await;
    let who = identity("sub-new", Some("SYN0100"), Some("new.person@example.test"));
    let (created, seen) = login(&t, &who, ON).await;
    let LoginOutcome::Created { user, namespace_id } = created.expect("created") else {
        panic!("not created");
    };
    assert_eq!(seen, Some(LoginAccount::New));
    let rows = t
        .json_rows(&format!(
            "SELECT username, display_name, kind, is_admin, auth_source, employee_number \
             FROM forge.users WHERE id = {}",
            user.get()
        ))
        .await;
    assert_eq!(rows[0]["username"], "newperson", "lower-cased ASCII");
    assert_eq!(rows[0]["display_name"], "New Person");
    assert_eq!(rows[0]["kind"], "human");
    assert_eq!(rows[0]["is_admin"], false);
    assert_eq!(rows[0]["auth_source"], "local");
    assert_eq!(rows[0]["employee_number"], "SYN0100");
    let audit = t
        .json_rows(
            "SELECT action, details FROM forge.audit_log \
             WHERE action IN ('user.created', 'user.identity_linked') ORDER BY id",
        )
        .await;
    assert_eq!(audit.len(), 2, "{audit:?}");
    assert_eq!(audit[0]["action"], "user.created");
    assert_eq!(audit[0]["details"]["via"], "oidc");
    assert_eq!(audit[0]["details"]["namespace_id"], namespace_id.get());
    assert_eq!(audit[1]["details"]["via"], "first_login");
    assert_eq!(identities(&t).await, 1);
    t.drop_now().await;
}

/// Every refusal of the link step (ADR-0021 §4): `FG-AUTH-0014` with its
/// reason, and nothing written.
#[tokio::test]
async fn identity_link_conflicts_name_their_reason() {
    let t = FreshDb::migrated().await;
    let _alice = person(&t, "alice", "SYN0001", false, UserKind::Human).await;
    let _root = person(&t, "root", "SYN0002", true, UserKind::Human).await;
    let bob = person(&t, "bob", "SYN0003", false, UserKind::Human).await;
    let _taken = person(&t, "newperson", "SYN0009", false, UserKind::Human).await;
    // bob already has an identity of this issuer.
    let _ = link_by_operator(&t.db, &system("req-op"), bob, ISSUER, "sub-bob")
        .await
        .expect("operator link");
    let users_before = scalar(&t, "SELECT count(*) FROM forge.users").await;
    let ids_before = identities(&t).await;

    let unverified = LoginIdentity {
        email_verified: false,
        ..identity("s1", Some("SYN0001"), Some("alice@example.test"))
    };
    let no_username = LoginIdentity {
        preferred_username: None,
        ..identity("s2", Some("SYN0200"), Some("x@example.test"))
    };
    let reserved = LoginIdentity {
        preferred_username: Some("api"),
        ..identity("s3", Some("SYN0201"), Some("y@example.test"))
    };
    let new_unverified = LoginIdentity {
        email_verified: false,
        ..identity("s4", Some("SYN0202"), Some("z@example.test"))
    };
    let cases: Vec<(LoginIdentity<'_>, LinkPolicy, &str)> = vec![
        // an e-mail alone never links
        (
            identity("s0", None, Some("alice@example.test")),
            ON,
            "employee_number_missing",
        ),
        (identity("s1", Some("SYN0001"), None), ON, "email_missing"),
        (unverified, ON, "email_unverified"),
        (
            identity("s1", Some("SYN0001"), Some("other@example.test")),
            ON,
            "email_mismatch",
        ),
        (
            identity("s1", Some("SYN0002"), Some("root@example.test")),
            ON,
            "privileged_account",
        ),
        (
            identity("s1", Some("SYN0003"), Some("bob@example.test")),
            ON,
            "employee_number_linked",
        ),
        (
            identity("s1", Some("SYN0001"), Some("alice@example.test")),
            LinkPolicy { auto_link: false },
            "auto_link_disabled",
        ),
        (no_username, ON, "username_unavailable"),
        (reserved, ON, "username_unavailable"),
        // preferred_username "NewPerson" → "newperson", which is taken
        (
            identity("s5", Some("SYN0203"), Some("fresh@example.test")),
            ON,
            "username_unavailable",
        ),
        (new_unverified, ON, "email_unverified"),
        (identity("s6", Some("SYN0204"), None), ON, "email_missing"),
    ];
    for (who, policy, expected) in cases {
        let (result, _) = login(&t, &who, policy).await;
        let err = result.expect_err(expected);
        assert_eq!(
            err.code(),
            ErrorCode::IdentityLinkConflict,
            "{expected}: {err}"
        );
        assert_eq!(reason(&err), expected, "{err}");
        assert!(!err.to_string().contains("example.test"), "{err}");
    }
    // e-mail taken by another user (a new user would collide).
    let taken_email = LoginIdentity {
        preferred_username: Some("carol"),
        ..identity("s7", Some("SYN0205"), Some("Alice@Example.test"))
    };
    let err = login(&t, &taken_email, ON)
        .await
        .0
        .expect_err("email taken");
    assert_eq!(reason(&err), "email_taken", "{err}");

    assert_eq!(
        scalar(&t, "SELECT count(*) FROM forge.users").await,
        users_before
    );
    assert_eq!(identities(&t).await, ids_before);
    t.drop_now().await;
}

/// The admission decision runs inside the change with the account found;
/// its error comes back unchanged and nothing is written.
#[tokio::test]
async fn the_admission_sees_the_account_and_a_denial_writes_nothing() {
    let t = FreshDb::migrated().await;
    let robot = person(&t, "robot", "SYN0300", false, UserKind::Bot).await;
    let alice = person(&t, "alice", "SYN0001", false, UserKind::Human).await;
    let _ = users::disable(&t.db, &system("req-disable"), alice)
        .await
        .expect("disable");
    let deny = || ForgeError::new(ErrorCode::LoginNotPermitted).with_context("reason", "test");
    for (who, expected) in [
        (
            identity("s1", Some("SYN0300"), Some("robot@example.test")),
            LoginAccount::NotHuman,
        ),
        (
            identity("s2", Some("SYN0001"), Some("alice@example.test")),
            LoginAccount::Disabled,
        ),
        (
            identity("s3", Some("SYN0400"), Some("n@example.test")),
            LoginAccount::New,
        ),
    ] {
        let mut seen = None;
        let err = login_decided(&t.db, &ctx(Actor::Anonymous, "req-a"), &who, ON, |a| {
            seen = Some(a);
            Err(deny())
        })
        .await
        .expect_err("denied");
        assert_eq!(err.code(), ErrorCode::LoginNotPermitted);
        assert_eq!(seen, Some(expected));
    }
    // A linked identity of a disabled user is decided as Disabled.
    let _ = robot;
    let _ = link_by_operator(&t.db, &system("req-op"), alice, ISSUER, "sub-alice")
        .await
        .expect("link");
    let (_, seen) = login(&t, &identity("sub-alice", None, None), ON).await;
    assert_eq!(seen, Some(LoginAccount::Disabled));
    assert_eq!(
        scalar(
            &t,
            "SELECT count(*) FROM forge.users WHERE username = 'newperson'"
        )
        .await,
        0
    );
    assert_eq!(identities(&t).await, 1);
    t.drop_now().await;
}

#[tokio::test]
async fn the_operator_links_only_as_system_and_reports_conflicts() {
    let t = FreshDb::migrated().await;
    let root = person(&t, "root", "SYN0002", true, UserKind::Human).await;
    let bob = person(&t, "bob", "SYN0003", false, UserKind::Human).await;
    let err = link_by_operator(
        &t.db,
        &ctx(Actor::User(root), "req-x"),
        root,
        ISSUER,
        "sub-root",
    )
    .await
    .expect_err("not system");
    assert_eq!(err.code(), ErrorCode::Internal);
    assert_eq!(reason(&err), "operator_not_system");

    let first = link_by_operator(&t.db, &system("req-1"), root, ISSUER, "sub-root").await;
    assert_eq!(first.expect("linked"), OperatorLink::Linked);
    let again = link_by_operator(&t.db, &system("req-2"), root, ISSUER, "sub-root").await;
    assert_eq!(again.expect("same"), OperatorLink::AlreadyLinked);
    let err = link_by_operator(&t.db, &system("req-3"), bob, ISSUER, "sub-root")
        .await
        .expect_err("another user's");
    assert_eq!(reason(&err), "subject_linked");
    let err = link_by_operator(&t.db, &system("req-4"), root, ISSUER, "sub-other")
        .await
        .expect_err("one per issuer");
    assert_eq!(reason(&err), "already_linked");
    let err = link_by_operator(&t.db, &system("req-5"), UserId::new(999_999), ISSUER, "s")
        .await
        .expect_err("unknown user");
    assert_eq!(err.code(), ErrorCode::NotFound);
    let err = link_by_operator(&t.db, &system("req-6"), bob, "", "s")
        .await
        .expect_err("empty issuer");
    assert_eq!(err.code(), ErrorCode::UserRequestInvalid);
    let rows = t
        .json_rows(
            "SELECT actor_kind, details FROM forge.audit_log WHERE action = 'user.identity_linked'",
        )
        .await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["details"]["via"], "operator");
    assert_eq!(rows[0]["actor_kind"], "system");
    // The administrator signs in through the operator's link.
    let (found, seen) = login(&t, &identity("sub-root", None, None), ON).await;
    assert_eq!(
        found.expect("existing"),
        LoginOutcome::Existing { user: root }
    );
    assert_eq!(seen, Some(LoginAccount::Active));
    t.drop_now().await;
}
