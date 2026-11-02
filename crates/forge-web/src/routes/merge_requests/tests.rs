//! Unit tests of the merge request routes' parsing (branch names, ids,
//! the list query).

use forge_core::{MergeRequestState, ProjectId};
use forge_errors::{ErrorCode, ForgeError};

use super::loaders::{branch_param, ids, version_ids};
use super::{page_query, DEFAULT_LIMIT};

fn rule(err: &ForgeError) -> String {
    err.to_string()
}

#[test]
fn branch_params_are_branches_only() {
    assert_eq!(
        branch_param("source_branch", "feature/x")
            .expect("short")
            .as_str(),
        "refs/heads/feature/x"
    );
    assert_eq!(
        branch_param("source_branch", "refs/heads/main")
            .expect("full")
            .as_str(),
        "refs/heads/main"
    );
    for (raw, why) in [
        ("refs/tags/v1", "not_a_branch"),
        ("refs/merge-requests/1/head", "not_a_branch"),
        ("-rf", "invalid_ref_name"),
        ("a..b", "invalid_ref_name"),
        ("", "invalid_ref_name"),
    ] {
        let err = branch_param("target_branch", raw).expect_err(raw);
        assert_eq!(err.code(), ErrorCode::RequestInvalid, "{raw}");
        assert!(rule(&err).contains(why), "{raw}: {err}");
    }
}

#[test]
fn ids_and_queries_are_strict() {
    let p = "/api/v1/projects/7/merge-requests/3";
    assert_eq!(ids(p, "").expect("ids"), (ProjectId::new(7), 3));
    assert_eq!(
        ids("/api/v1/projects/7/merge-requests/3/close", "/close").expect("ids"),
        (ProjectId::new(7), 3)
    );
    for bad in [
        "/api/v1/projects/7/merge-requests/0",
        "/api/v1/projects/7/merge-requests/03",
        "/api/v1/projects/7/merge-requests/3/x",
        "/api/v1/projects/x/merge-requests/3",
    ] {
        assert!(ids(bad, "").is_err(), "{bad}");
    }
    assert_eq!(
        page_query(None).expect("defaults"),
        (Some(MergeRequestState::Open), DEFAULT_LIMIT, None)
    );
    assert_eq!(
        page_query(Some("state=all&limit=5&before=9")).expect("all"),
        (None, 5, Some(9))
    );
    for bad in [
        "state=draft",
        "limit=101",
        "limit=0",
        "before=-1",
        "x=1",
        "limit=1&limit=2",
    ] {
        assert!(page_query(Some(bad)).is_err(), "{bad}");
    }
}

/// M3 R2: `…/{iid}/versions/{version}/diff` — both numbers plain positive
/// decimals, nothing after `/diff`.
#[test]
fn version_diff_paths_name_an_iid_and_a_version() {
    assert_eq!(
        version_ids("/api/v1/projects/7/merge-requests/3/versions/2/diff").expect("ids"),
        (ProjectId::new(7), 3, 2)
    );
    for (raw, param) in [
        (
            "/api/v1/projects/7/merge-requests/3/versions/0/diff",
            "version",
        ),
        (
            "/api/v1/projects/7/merge-requests/3/versions/02/diff",
            "version",
        ),
        (
            "/api/v1/projects/7/merge-requests/3/versions/x/diff",
            "version",
        ),
        (
            "/api/v1/projects/7/merge-requests/3/versions/2/diff/x",
            "version",
        ),
        (
            "/api/v1/projects/7/merge-requests/3/versions/2/3/diff",
            "version",
        ),
        ("/api/v1/projects/7/merge-requests/a/versions/2/diff", "iid"),
        ("/api/v1/projects/x/merge-requests/3/versions/2/diff", "id"),
    ] {
        let err = version_ids(raw).expect_err(raw);
        assert_eq!(err.code(), ErrorCode::RequestInvalid, "{raw}");
        assert!(rule(&err).contains(param), "{raw}: {err}");
    }
}
