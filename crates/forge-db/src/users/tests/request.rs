//! users::create request rules: field limits (FG-AUTH-0006), identifiers
//! (FG-AUTH-0007), accounts that are not people, a group's top-level path,
//! and disabling an unknown user.

use forge_core::ids::UserId;

use super::*;
use crate::audited_tests::sql;

fn human(name: &str) -> NewUser {
    NewUser::human(name, "Someone", &format!("{name}@example.test"))
}

#[tokio::test]
async fn an_invalid_user_request_is_user_request_invalid() {
    let t = FreshDb::migrated().await;
    let cases = [
        (
            NewUser {
                email: None,
                ..human("u1")
            },
            "email_missing",
        ),
        (
            NewUser {
                display_name: String::new(),
                ..human("u2")
            },
            "display_name_length",
        ),
        (
            NewUser {
                display_name: "x".repeat(256),
                ..human("u3")
            },
            "display_name_length",
        ),
        (
            NewUser {
                email: Some(String::new()),
                ..human("u4")
            },
            "email_length",
        ),
        (
            NewUser {
                email: Some(format!("{}@example.test", "e".repeat(320))),
                ..human("u5")
            },
            "email_length",
        ),
        (
            NewUser {
                employee_number: Some("n".repeat(65)),
                ..human("u6")
            },
            "employee_number_length",
        ),
        (
            NewUser {
                employee_number: Some(String::new()),
                ..human("u7")
            },
            "employee_number_length",
        ),
        (
            NewUser {
                external_id: Some("x".repeat(256)),
                ..human("u8")
            },
            "external_id_length",
        ),
        (
            NewUser {
                display_name: "a\0b".into(),
                ..human("u9")
            },
            "nul_character",
        ),
    ];
    for (new, reason) in cases {
        let request = format!("req-invalid-{}", new.username);
        let err = create(&t.db, &ctx(&request), &new).await.expect_err(reason);
        assert_reason(&err, ErrorCode::UserRequestInvalid, reason);
        assert_eq!(err.code().http_status(), 400);
        assert!(!chain(&err).contains("example.test"), "no value: {err}");
        let mut su = t.superuser().await;
        assert!(audit_rows(&mut su, &request).await.is_empty(), "{reason}");
    }
    // The limits themselves are accepted (the CHECKs agree).
    let at_limits = NewUser {
        display_name: "x".repeat(255),
        email: Some(format!("{}@example.test", "e".repeat(307))),
        employee_number: Some("n".repeat(64)),
        external_id: Some("x".repeat(255)),
        ..human("limits")
    };
    create(&t.db, &ctx("req-limits"), &at_limits)
        .await
        .expect("limits are inclusive");
    let mut su = t.superuser().await;
    assert_eq!(rows_of(&mut su, "users").await, 1);
    t.drop_now().await;
}

#[tokio::test]
async fn a_taken_employee_number_or_external_id_is_user_identifier_taken() {
    let t = FreshDb::migrated().await;
    let first = NewUser {
        employee_number: Some("N-100".into()),
        external_id: Some("uid-100".into()),
        auth_source: AuthSource::Ldap,
        ..human("first")
    };
    create(&t.db, &ctx("req-first"), &first)
        .await
        .expect("first");
    let cases = [
        (
            NewUser {
                employee_number: Some("N-100".into()),
                ..human("second")
            },
            "employee_number_taken",
        ),
        (
            NewUser {
                external_id: Some("uid-100".into()),
                auth_source: AuthSource::Ldap,
                ..human("third")
            },
            "external_id_taken",
        ),
    ];
    for (new, reason) in cases {
        let request = format!("req-{}", new.username);
        let err = create(&t.db, &ctx(&request), &new).await.expect_err(reason);
        assert_reason(&err, ErrorCode::UserIdentifierTaken, reason);
        assert_eq!(err.code().http_status(), 409);
        let text = chain(&err);
        assert!(
            !text.contains("N-100") && !text.contains("uid-100"),
            "{text}"
        );
        let mut su = t.superuser().await;
        assert!(audit_rows(&mut su, &request).await.is_empty(), "{reason}");
    }
    // An external id is unique per auth source only.
    let local = NewUser {
        external_id: Some("uid-100".into()),
        ..human("local")
    };
    create(&t.db, &ctx("req-local"), &local)
        .await
        .expect("same external id, other source");
    let mut su = t.superuser().await;
    assert_eq!(rows_of(&mut su, "users").await, 2);
    t.drop_now().await;
}

#[tokio::test]
async fn a_service_account_needs_no_email() {
    let t = FreshDb::migrated().await;
    let svc = NewUser {
        kind: UserKind::ServiceAccount,
        email: None,
        external_id: Some("cn=ci".into()),
        auth_source: AuthSource::Ldap,
        is_admin: true,
        ..human("svc-ci")
    };
    let created = create(&t.db, &ctx("req-svc"), &svc)
        .await
        .expect("service account");
    let mut su = t.superuser().await;
    let row: (String, Option<String>, String, bool) =
        sqlx::query_as("SELECT kind, email, auth_source, is_admin FROM forge.users WHERE id = $1")
            .bind(created.user_id.get())
            .fetch_one(&mut su)
            .await
            .expect("row");
    assert_eq!(row, ("service_account".into(), None, "ldap".into(), true));
    let details: serde_json::Value =
        sqlx::query_scalar("SELECT details FROM forge.audit_log WHERE request_id = 'req-svc'")
            .fetch_one(&mut su)
            .await
            .expect("details");
    assert_eq!(
        details,
        json!({"kind": "service_account", "namespace_id": created.namespace_id.get(), "is_admin": true})
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_top_level_group_takes_the_username_but_a_nested_one_does_not() {
    let t = FreshDb::migrated().await;
    let acme = commit(
        &t,
        &[
            audit("group", 7, "succeeded"),
            sql(
                "INSERT INTO forge.namespaces (kind, owner_id, segment, full_path, depth) \
                 VALUES ('group', 7, 'acme', 'acme', 1)",
            ),
        ],
    )
    .await;
    acme.expect("seed group acme");
    commit(
        &t,
        &[
            audit("group", 8, "succeeded"),
            sql(
                "INSERT INTO forge.namespaces (parent_id, kind, owner_id, segment, full_path, \
                 depth) SELECT id, 'group', 8, 'bob', 'acme/bob', 2 FROM forge.namespaces \
                 WHERE full_path = 'acme'",
            ),
        ],
    )
    .await
    .expect("seed group acme/bob");
    let err = create(&t.db, &ctx("req-acme"), &human("acme"))
        .await
        .expect_err("a top-level group has the path");
    assert_reason(&err, ErrorCode::NamespacePathTaken, "sibling");
    create(&t.db, &ctx("req-bob"), &human("bob"))
        .await
        .expect("acme/bob is no sibling of a top-level bob");
    t.drop_now().await;
}

#[tokio::test]
async fn disabling_an_unknown_user_is_not_found() {
    let t = FreshDb::migrated().await;
    let err = disable(&t.db, &ctx("req-ghost"), UserId::new(424_242))
        .await
        .expect_err("no such user");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    assert!(err.to_string().contains("resource=user"), "{err}");
    assert_eq!(err.code().http_status(), 404);
    let mut su = t.superuser().await;
    assert!(audit_rows(&mut su, "req-ghost").await.is_empty());
    t.drop_now().await;
}
