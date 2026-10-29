use crate::{Role, Visibility};

#[test]
fn role_order_is_guest_to_owner() {
    assert_eq!(
        Role::ALL,
        &[
            Role::Guest,
            Role::Reporter,
            Role::Developer,
            Role::Maintainer,
            Role::Owner
        ]
    );
    assert!(Role::ALL.windows(2).all(|w| w[0] < w[1]), "ascending");
    assert_eq!(Role::ALL.iter().max(), Some(&Role::Owner));
}

#[test]
fn role_str_round_trips() {
    let names: Vec<&str> = Role::ALL.iter().map(|r| r.as_str()).collect();
    assert_eq!(
        names,
        ["guest", "reporter", "developer", "maintainer", "owner"]
    );
    for role in Role::ALL {
        assert_eq!(Role::parse(role.as_str()), Some(*role));
    }
    for bad in ["", "Owner", "admin", "owner "] {
        assert_eq!(Role::parse(bad), None, "{bad:?}");
    }
}

#[test]
fn visibility_str_round_trips() {
    for (v, s) in [
        (Visibility::Private, "private"),
        (Visibility::Internal, "internal"),
    ] {
        assert_eq!(v.as_str(), s);
        assert_eq!(Visibility::parse(s), Some(v));
    }
    assert_eq!(Visibility::parse("public"), None, "no public in M1");
}
