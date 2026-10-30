use crate::{RepositoryKind, RepositoryState};

#[test]
fn repository_kinds_are_spec_and_system() {
    let names: Vec<&str> = RepositoryKind::ALL.iter().map(|k| k.as_str()).collect();
    assert_eq!(names, ["spec", "system"]);
    for kind in RepositoryKind::ALL {
        assert_eq!(RepositoryKind::parse(kind.as_str()), Some(*kind));
    }
    for not_a_kind in ["", "Spec", "SYSTEM", "spec ", "docs", "wiki"] {
        assert_eq!(RepositoryKind::parse(not_a_kind), None, "{not_a_kind:?}");
    }
}

#[test]
fn repository_states_are_provisioning_and_ready() {
    let names: Vec<&str> = RepositoryState::ALL.iter().map(|s| s.as_str()).collect();
    assert_eq!(names, ["provisioning", "ready"]);
    for state in RepositoryState::ALL {
        assert_eq!(RepositoryState::parse(state.as_str()), Some(*state));
    }
    for not_a_state in ["", "Ready", "deleted", "failed", "ready "] {
        assert_eq!(RepositoryState::parse(not_a_state), None, "{not_a_state:?}");
    }
}
