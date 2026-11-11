//! The hidden ref namespace of merge requests (ADR-0024 §2, M3 R1b):
//! `refs/merge-requests/<iid>/v<n>` (one per version, never overwritten),
//! `refs/merge-requests/<iid>/head` (the latest version) and (M4 C7)
//! `refs/merge-requests/<iid>/merge` (the merge result of the head into the
//! target tip).
//!
//! One prefix ([`HIDDEN_REF_PREFIX`]) drives every place that must leave
//! these refs out, so they can never disagree (unit test
//! `hidden_ref_settings_derive_from_one_prefix`):
//!
//! - `transfer.hideRefs` in both fixed `-c` sets of Smart HTTP
//!   (`UPLOAD_PACK_CONFIG` and `RECEIVE_PACK_CONFIG`): neither
//!   advertisement shows them, and receive-pack refuses to update them;
//! - `--exclude=<glob>` ([`HIDDEN_REF_GLOB`]) right before both `--all`
//!   walks of the want check (ADR-0007 amendment, M3 R1b): an object only a
//!   hidden ref reaches is refused like an unreferenced one.
//!
//! M4 C4 (ADR-0029 §3): the pipeline refs `refs/pipelines/<id>/head` (the
//! pinned commit of one CI pipeline, written by the job fetch route) are
//! hidden the same way ([`PIPELINE_REF_PREFIX`], [`PIPELINE_REF_GLOB`]): a
//! person's clone never sees them and no push can write them. Only a job's
//! own fetch (`Git::upload_pack_pinned`) un-hides exactly its pipeline's ref.
//!
//! The ref names are built only here, from a positive iid and version
//! number (or pipeline id), as validated [`RefName`]s.

use forge_core::RefName;
use forge_errors::{ErrorCode, ForgeError};

macro_rules! hidden_prefix {
    () => {
        "refs/merge-requests/"
    };
}

/// Every ref under this prefix is hidden from Smart HTTP and left out of
/// the want check. The `transfer.hideRefs` value (git strips the trailing
/// `/` and matches whole path components).
pub const HIDDEN_REF_PREFIX: &str = hidden_prefix!();

/// The `--exclude` pattern of the want check: every ref under
/// [`HIDDEN_REF_PREFIX`] (`*` matches `/` in git's ref exclusion).
pub(crate) const HIDDEN_REF_GLOB: &str = concat!(hidden_prefix!(), "*");

macro_rules! pipeline_prefix {
    () => {
        "refs/pipelines/"
    };
}

/// The pipeline refs (M4 C4): hidden from Smart HTTP and left out of the
/// want check like [`HIDDEN_REF_PREFIX`].
pub const PIPELINE_REF_PREFIX: &str = pipeline_prefix!();

/// The `--exclude` pattern of the pipeline refs in the want check.
pub(crate) const PIPELINE_REF_GLOB: &str = concat!(pipeline_prefix!(), "*");

/// `refs/pipelines/<pipeline_id>/head`: the pinned commit a pipeline's jobs
/// fetch. A non-positive id is a forge bug (`FG-SYS-0001
/// reason=pipeline_ref_number`).
pub fn pipeline_head_ref(pipeline_id: i64) -> Result<RefName, ForgeError> {
    if pipeline_id <= 0 {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "pipeline_ref_number")
            .with_context("what", "pipeline_id"));
    }
    RefName::parse(&format!("{PIPELINE_REF_PREFIX}{pipeline_id}/head"))
}

fn bad_number(what: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "merge_request_ref_number")
        .with_context("what", what)
}

/// `refs/merge-requests/<iid>/` (the prefix of one merge request's refs).
pub fn merge_request_ref_prefix(iid: i64) -> Result<String, ForgeError> {
    if iid <= 0 {
        return Err(bad_number("iid"));
    }
    Ok(format!("{HIDDEN_REF_PREFIX}{iid}/"))
}

/// `refs/merge-requests/<iid>/v<version>`. A non-positive number is a
/// forge bug (`FG-SYS-0001 reason=merge_request_ref_number`).
pub fn merge_request_version_ref(iid: i64, version: i64) -> Result<RefName, ForgeError> {
    if version <= 0 {
        return Err(bad_number("version"));
    }
    RefName::parse(&format!("{}v{version}", merge_request_ref_prefix(iid)?))
}

/// `refs/merge-requests/<iid>/head`.
pub fn merge_request_head_ref(iid: i64) -> Result<RefName, ForgeError> {
    RefName::parse(&format!("{}head", merge_request_ref_prefix(iid)?))
}

/// `refs/merge-requests/<iid>/merge` (M4 C7): the merge result of the head
/// into the target tip (`crate::merge_result`). Under
/// [`HIDDEN_REF_PREFIX`], so hidden like the version refs.
pub fn merge_request_merge_ref(iid: i64) -> Result<RefName, ForgeError> {
    RefName::parse(&format!("{}merge", merge_request_ref_prefix(iid)?))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // tests may fail loudly

    use super::*;
    use crate::smart_http::{RECEIVE_PACK_CONFIG, UPLOAD_PACK_CONFIG};

    fn hide_refs(config: &[(&'static str, &'static str)]) -> Vec<&'static str> {
        config
            .iter()
            .filter(|(k, _)| *k == "transfer.hideRefs")
            .map(|(_, v)| *v)
            .collect()
    }

    #[test]
    fn hidden_ref_settings_derive_from_one_prefix() {
        assert_eq!(HIDDEN_REF_PREFIX, "refs/merge-requests/");
        assert_eq!(HIDDEN_REF_GLOB, format!("{HIDDEN_REF_PREFIX}*"));
        assert_eq!(PIPELINE_REF_PREFIX, "refs/pipelines/");
        assert_eq!(PIPELINE_REF_GLOB, format!("{PIPELINE_REF_PREFIX}*"));
        // Both services hide exactly the prefixes (one of them alone would
        // leave the refs and their ids in the other's advertisement).
        let both = [HIDDEN_REF_PREFIX, PIPELINE_REF_PREFIX];
        assert_eq!(hide_refs(UPLOAD_PACK_CONFIG), both);
        assert_eq!(hide_refs(RECEIVE_PACK_CONFIG), both);
    }

    #[test]
    fn pipeline_ref_names_are_built_from_positive_ids() {
        assert_eq!(
            pipeline_head_ref(42).expect("ref").as_str(),
            "refs/pipelines/42/head"
        );
        for bad in [0, -1] {
            let err = pipeline_head_ref(bad).expect_err("refused");
            assert_eq!(err.code(), ErrorCode::Internal);
        }
    }

    #[test]
    fn merge_request_ref_names_are_built_from_positive_numbers() {
        assert_eq!(
            merge_request_version_ref(3, 2).expect("ref").as_str(),
            "refs/merge-requests/3/v2"
        );
        assert_eq!(
            merge_request_head_ref(3).expect("ref").as_str(),
            "refs/merge-requests/3/head"
        );
        assert_eq!(
            merge_request_ref_prefix(12).expect("prefix"),
            "refs/merge-requests/12/"
        );
        for (iid, version) in [(0, 1), (-1, 1), (1, 0), (1, -3)] {
            let err = merge_request_version_ref(iid, version).expect_err("refused");
            assert_eq!(err.code(), ErrorCode::Internal);
        }
        assert!(merge_request_head_ref(0).is_err());
        assert_eq!(
            merge_request_merge_ref(3).expect("ref").as_str(),
            "refs/merge-requests/3/merge"
        );
        assert!(merge_request_merge_ref(0).is_err());
        assert!(merge_request_merge_ref(3)
            .expect("ref")
            .as_str()
            .starts_with(HIDDEN_REF_PREFIX));
        let name = merge_request_version_ref(1, 1).expect("ref");
        assert!(name.as_str().starts_with(HIDDEN_REF_PREFIX));
    }
}
