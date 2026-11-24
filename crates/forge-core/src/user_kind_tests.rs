use crate::{AdminSource, UserKind};

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

#[test]
fn admin_sources_are_the_check_of_users_admin_source() {
    let names: Vec<&str> = AdminSource::ALL.iter().map(|k| k.as_str()).collect();
    assert_eq!(names, ["cli", "oidc"]);
    for source in AdminSource::ALL {
        assert_eq!(AdminSource::parse(source.as_str()), Some(*source));
    }
    assert!(AdminSource::standing_admin(true, AdminSource::Cli));
    assert!(!AdminSource::standing_admin(true, AdminSource::Oidc));
    assert!(!AdminSource::standing_admin(false, AdminSource::Cli));
    for other in ["ldap", "OIDC", "", "keycloak"] {
        assert_eq!(AdminSource::parse(other), None, "{other:?}");
    }
}
