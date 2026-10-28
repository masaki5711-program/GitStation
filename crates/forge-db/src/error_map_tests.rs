//! Unit tests of the sqlx error mapping (no database needed).

use super::*;

fn reason_of(err: &ForgeError) -> String {
    err.to_string()
        .split(' ')
        .find_map(|kv| kv.strip_prefix("reason="))
        .unwrap_or_default()
        .trim_end_matches(':')
        .to_owned()
}

#[test]
fn xf002_from_an_audited_table_trigger_has_its_own_reason() {
    assert_eq!(
        audit_missing_reason(Some("audited_change")),
        "audited_change"
    );
    assert_eq!(
        audit_missing_reason(Some("audited_change_config")),
        "audited_change_config"
    );
}

#[test]
fn xf002_reasons_follow_the_constraint_and_an_unknown_one_is_not_no_audit_row() {
    assert_eq!(audit_missing_reason(None), "no_audit_row");
    assert_eq!(
        audit_missing_reason(Some("derived_fanout")),
        "derived_fanout"
    );
    assert_eq!(
        audit_missing_reason(Some("derived_progress")),
        "derived_progress"
    );
    assert_eq!(
        audit_missing_reason(Some("events_new_check")),
        "xf002_unknown"
    );
}

#[test]
fn transport_errors_are_unavailable_with_their_reason() {
    for (kind, reason) in [
        (ErrorKind::ConnectionRefused, "connect_refused"),
        (ErrorKind::TimedOut, "connect_timeout"),
        (ErrorKind::ConnectionReset, "connection_lost"),
        (ErrorKind::BrokenPipe, "connection_lost"),
    ] {
        let err = query_error("t.q", sqlx::Error::Io(std::io::Error::from(kind)));
        assert_eq!(err.code(), ErrorCode::DbUnavailable, "{err}");
        assert_eq!(reason_of(&err), reason, "{err}");
    }
    for (e, reason) in [
        (sqlx::Error::PoolTimedOut, "acquire_timeout"),
        (sqlx::Error::PoolClosed, "pool_closed"),
        (sqlx::Error::WorkerCrashed, "connection_lost"),
        (sqlx::Error::Protocol("x".into()), "connection_lost"),
    ] {
        let err = query_error("t.q", e);
        assert_eq!(err.code(), ErrorCode::DbUnavailable, "{err}");
        assert_eq!(reason_of(&err), reason, "{err}");
    }
}

#[test]
fn a_decode_error_never_carries_the_value() {
    let e = sqlx::Error::ColumnDecode {
        index: "\"kind\"".to_owned(),
        source: "invalid value \"tok-5e1f-secret\" for enum Kind".into(),
    };
    let err = query_error("t.q", e);
    assert_eq!(err.code(), ErrorCode::DbQueryFailed);
    let text = err.to_string();
    assert!(!text.contains("tok-5e1f-secret"), "{text}");
    assert!(text.contains("driver_error=column_decode"), "{text}");
    assert!(text.contains("column=\"kind\""), "{text}");
    assert!(std::error::Error::source(&err).is_none());
    let err = query_error(
        "t.q",
        sqlx::Error::Decode("value \"x-9\" out of range".into()),
    );
    assert!(!err.to_string().contains("x-9"), "{err}");
}

#[test]
fn row_not_found_is_a_query_failure_with_its_own_reason() {
    let err = query_error("t.q", sqlx::Error::RowNotFound);
    assert_eq!(err.code(), ErrorCode::DbQueryFailed);
    assert_eq!(reason_of(&err), "row_not_found");
}

#[test]
fn sqlstates_map_to_the_documented_reasons() {
    for (state, unavailable, reason) in [
        ("57014", false, "statement_timeout"),
        ("55P03", false, "lock_timeout"),
        ("40001", false, "serialization"),
        ("40P01", false, "deadlock"),
        ("23505", false, "constraint"),
        ("42P01", false, "server_error"),
        ("57P01", true, "server_shutdown"),
        ("57P03", true, "server_shutdown"),
        ("53300", true, "too_many_connections"),
        ("08006", true, "connection_lost"),
        ("53100", true, "resource_exhausted"),
        ("53200", true, "resource_exhausted"),
        ("58030", true, "server_fault"),
        ("XX000", true, "server_fault"),
        ("25006", true, "read_only"),
        ("25P03", false, "idle_in_transaction_timeout"),
        ("28P01", true, "auth_failed"),
        ("42501", false, "permission_denied"),
        ("XF001", false, "append_only"),
        ("XF002", false, "audit_missing"),
    ] {
        let class = by_sqlstate(state, On::Write);
        let (is_unavailable, r) = match class {
            Class::Unavailable(r) => (true, r),
            Class::Query(r) => (false, r),
            Class::AppendOnly => (false, "append_only"),
            Class::AuditMissing => (false, "audit_missing"),
        };
        assert_eq!((is_unavailable, r), (unavailable, reason), "{state}");
    }
}

#[test]
fn a_read_only_rejection_depends_on_the_pool() {
    assert!(matches!(
        by_sqlstate("25006", On::Read),
        Class::Query("write_on_read_pool")
    ));
    assert!(matches!(
        by_sqlstate("25006", On::Write),
        Class::Unavailable("read_only")
    ));
    assert!(matches!(
        by_sqlstate("42501", On::Read),
        Class::Query("permission_denied")
    ));
}

#[test]
fn only_data_free_classes_keep_the_server_message() {
    assert!(message_is_safe("42P01"));
    assert!(message_is_safe("57P01"));
    assert!(!message_is_safe("42601"), "a syntax error quotes the token");
    assert!(!message_is_safe("55006"), "object in use may name data");
    assert!(
        !message_is_safe("23505"),
        "unique violation detail quotes the key"
    );
    assert!(!message_is_safe("22P02"), "invalid input quotes the value");
}
