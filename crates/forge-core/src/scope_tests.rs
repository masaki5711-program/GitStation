use crate::Scope;

const ALL: [(Scope, &str); 10] = [
    (Scope::ReadRepository, "read_repository"),
    (Scope::WriteRepository, "write_repository"),
    (Scope::ReadApi, "read_api"),
    (Scope::Api, "api"),
    (Scope::Admin, "admin"),
    (Scope::MergeExecute, "merge_execute"),
    (Scope::MergeApprove, "merge_approve"),
    (Scope::UserEmail, "user_email"),
    (Scope::UserEmployeeNumber, "user_employee_number"),
    (Scope::ActAs, "act_as"),
];

#[test]
fn scope_str_round_trips() {
    for (scope, name) in ALL {
        assert_eq!(scope.as_str(), name);
    }
    let names: std::collections::HashSet<_> = ALL.iter().map(|(s, _)| s.as_str()).collect();
    assert_eq!(names.len(), ALL.len(), "names are distinct");
}

#[test]
fn scope_parse_accepts_exactly_the_names() {
    for (scope, name) in ALL {
        assert_eq!(Scope::parse(name), Some(scope));
    }
    assert_eq!(Scope::ALL.len(), ALL.len(), "ALL lists every scope");
    for other in ["", "API", "read-api", "teleport", " api"] {
        assert_eq!(Scope::parse(other), None, "{other:?}");
    }
}

#[test]
fn scope_satisfies_is_unchanged_after_the_move() {
    let implied = [
        (Scope::WriteRepository, Scope::ReadRepository),
        (Scope::Api, Scope::ReadApi),
    ];
    for (held, _) in ALL {
        for (required, _) in ALL {
            let expected = held == required || implied.contains(&(held, required));
            assert_eq!(
                held.satisfies(required),
                expected,
                "{held:?} vs {required:?}"
            );
        }
    }
}

/// ADR-0024 §8.5 (M3 R4): a merge scope is satisfied only by itself — not
/// by `api`, not by `admin` — and satisfies no other scope (it never stands
/// in for `api` / `read_api`). The whole `ALL × ALL` table.
#[test]
fn merge_scopes_are_satisfied_only_by_themselves() {
    for held in Scope::ALL {
        for required in Scope::ALL {
            let merge = [held, required]
                .iter()
                .any(|s| matches!(s, Scope::MergeExecute | Scope::MergeApprove));
            if merge {
                assert_eq!(
                    held.satisfies(required),
                    held == required,
                    "{held:?} vs {required:?}"
                );
            }
        }
    }
    assert!(!Scope::Api.satisfies(Scope::MergeExecute));
    assert!(!Scope::Admin.satisfies(Scope::MergeExecute));
    assert!(!Scope::MergeExecute.satisfies(Scope::Api));
    assert!(!Scope::MergeExecute.satisfies(Scope::ReadApi));
    // M3 R5: `merge_approve` likewise, and the two never stand in for each
    // other.
    assert!(!Scope::Api.satisfies(Scope::MergeApprove));
    assert!(!Scope::Admin.satisfies(Scope::MergeApprove));
    assert!(!Scope::MergeExecute.satisfies(Scope::MergeApprove));
    assert!(!Scope::MergeApprove.satisfies(Scope::MergeExecute));
    assert!(!Scope::MergeApprove.satisfies(Scope::Api));
}

/// M3 S0 (plan 11 §5 S0): `user_email` only adds the addresses to what a
/// token may already read. It satisfies nothing else — a `user_email`-only
/// token reads no API — and nothing else satisfies it, `api` and `admin`
/// included.
#[test]
fn user_email_scope_does_not_imply_read_api() {
    for other in Scope::ALL {
        if other == Scope::UserEmail {
            continue;
        }
        assert!(
            !Scope::UserEmail.satisfies(other),
            "user_email vs {other:?}"
        );
        assert!(
            !other.satisfies(Scope::UserEmail),
            "{other:?} vs user_email"
        );
    }
    assert!(Scope::UserEmail.satisfies(Scope::UserEmail));
    assert!(!Scope::UserEmail.satisfies(Scope::ReadApi));
}

/// M3 I1c (plan 11 Q7): `user_employee_number` is shaped like
/// `user_email` — it satisfies nothing else and nothing else satisfies it.
#[test]
fn user_employee_number_scope_is_an_addition_only() {
    for other in Scope::ALL {
        if other == Scope::UserEmployeeNumber {
            continue;
        }
        assert!(!Scope::UserEmployeeNumber.satisfies(other), "{other:?}");
        assert!(!other.satisfies(Scope::UserEmployeeNumber), "{other:?}");
    }
    assert!(Scope::UserEmployeeNumber.satisfies(Scope::UserEmployeeNumber));
}

/// M3 S2 (ADR-0025 §2): `act_as` satisfies only itself; no other scope —
/// `api` and `admin` included — satisfies it.
#[test]
fn act_as_scope_satisfies_only_itself() {
    for other in Scope::ALL {
        if other == Scope::ActAs {
            continue;
        }
        assert!(!Scope::ActAs.satisfies(other), "{other:?}");
        assert!(!other.satisfies(Scope::ActAs), "{other:?}");
    }
    assert!(Scope::ActAs.satisfies(Scope::ActAs));
}
