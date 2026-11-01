use crate::RepositoryState;

#[test]
fn repository_states_are_provisioning_ready_and_purging() {
    let names: Vec<&str> = RepositoryState::ALL.iter().map(|s| s.as_str()).collect();
    assert_eq!(names, ["provisioning", "ready", "purging"]);
    for state in RepositoryState::ALL {
        assert_eq!(RepositoryState::parse(state.as_str()), Some(*state));
    }
    for not_a_state in ["", "Ready", "deleted", "failed", "ready ", "Purging"] {
        assert_eq!(RepositoryState::parse(not_a_state), None, "{not_a_state:?}");
    }
}
