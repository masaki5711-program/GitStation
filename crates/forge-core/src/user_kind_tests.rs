use crate::UserKind;

#[test]
fn a_device_is_not_a_user_kind() {
    let names: Vec<&str> = UserKind::ALL.iter().map(|k| k.as_str()).collect();
    assert_eq!(names, ["human", "service_account", "bot"]);
    for kind in UserKind::ALL {
        assert_eq!(UserKind::parse(kind.as_str()), Some(*kind));
    }
    for not_a_kind in ["device", "Human", "human ", "", "service-account", "edge"] {
        assert_eq!(UserKind::parse(not_a_kind), None, "{not_a_kind:?}");
    }
}
