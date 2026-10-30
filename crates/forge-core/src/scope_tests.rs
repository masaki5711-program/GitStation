use crate::Scope;

const ALL: [(Scope, &str); 5] = [
    (Scope::ReadRepository, "read_repository"),
    (Scope::WriteRepository, "write_repository"),
    (Scope::ReadApi, "read_api"),
    (Scope::Api, "api"),
    (Scope::Admin, "admin"),
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
