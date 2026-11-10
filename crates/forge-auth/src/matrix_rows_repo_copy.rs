//! The permission matrix rows of a whole copy of a repository (plan 06
//! D-5 safety net, ADR-0026 decision of 2026-11-10): `RepoRead` on
//! `Resource::RepoCopy` — the Smart HTTP upload-pack advertisement
//! (`GET {path}.git/info/refs?service=git-upload-pack`), the upload-pack
//! RPC (`POST {path}.git/git-upload-pack`: clone and fetch, protocol v0 and
//! v2) and the archive (`GET|HEAD {path}/-/archive/{ref}.{zip|tar.gz}`),
//! each decided again by `forge_auth::authorize_repo_copy` after the
//! dispatcher's `RepoRead` proof, with the route's measurement of the
//! default branch's tip (`CopyMark`).
//!
//! First the project read exactly as `RepoRead` on the project (role,
//! visibility, `read_repository`); then an AI agent audience — a token
//! issued with `is_agent`, a service account, a bot — is refused
//! (`FG-AUTH-0022`) unless the tip was measured `Open`: a confidential file
//! at the tip, an unborn default branch, a measurement that failed or timed
//! out, or none at all. A person (session or token, administrators too) is
//! never refused by the mark. Push (`RepoPush`) is no copy: its rows are
//! the base matrix's, unchanged.

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Fixture as F, Row};
use crate::model::CopyMark as M;

const fn row(action: A, actor: W, fixture: F, expect: Expect) -> Row {
    Row {
        action,
        actor,
        fixture,
        expect,
    }
}

const fn deny(code: E) -> Expect {
    Expect::Deny(code)
}

use Expect::Allow;

#[rustfmt::skip]
pub(crate) const REPO_COPY_ROWS: &[Row] = &[
    // People: never refused by the mark (not measured; any mark).
    row(A::RepoRead, W::Reporter, F::RepoCopy(M::NotMeasured), Allow),
    row(A::RepoRead, W::Reporter, F::RepoCopy(M::Confidential), Allow),
    row(A::RepoRead, W::Developer, F::RepoCopy(M::Undecided), Allow),
    row(A::RepoRead, W::DevTokenReadRepository, F::RepoCopy(M::NotMeasured), Allow),
    row(A::RepoRead, W::DevTokenReadRepository, F::RepoCopy(M::Confidential), Allow),
    row(A::RepoRead, W::DevTokenWriteRepository, F::RepoCopy(M::Unborn), Allow),
    row(A::RepoRead, W::Admin, F::RepoCopy(M::Confidential), Allow),
    row(A::RepoRead, W::NonMember, F::InternalRepoCopy(M::Confidential), Allow),
    // Agents (an `is_agent` token, a service account, a bot): only an
    // open tip.
    row(A::RepoRead, W::AgentTokenReadRepository, F::RepoCopy(M::Open), Allow),
    row(A::RepoRead, W::AgentTokenWriteRepository, F::RepoCopy(M::Open), Allow),
    row(A::RepoRead, W::ServiceAccountTokenReadRepository, F::RepoCopy(M::Open), Allow),
    row(A::RepoRead, W::BotTokenReadRepository, F::RepoCopy(M::Open), Allow),
    row(A::RepoRead, W::AgentTokenReadRepository, F::RepoCopy(M::Confidential), deny(E::RepositoryCopyWithheld)),
    row(A::RepoRead, W::AgentTokenReadRepository, F::RepoCopy(M::Unborn), deny(E::RepositoryCopyWithheld)),
    row(A::RepoRead, W::AgentTokenReadRepository, F::RepoCopy(M::Undecided), deny(E::RepositoryCopyWithheld)),
    row(A::RepoRead, W::AgentTokenReadRepository, F::RepoCopy(M::NotMeasured), deny(E::RepositoryCopyWithheld)),
    row(A::RepoRead, W::AgentTokenWriteRepository, F::RepoCopy(M::Confidential), deny(E::RepositoryCopyWithheld)),
    row(A::RepoRead, W::ServiceAccountTokenReadRepository, F::RepoCopy(M::Confidential), deny(E::RepositoryCopyWithheld)),
    row(A::RepoRead, W::ServiceAccountTokenReadRepository, F::RepoCopy(M::Undecided), deny(E::RepositoryCopyWithheld)),
    row(A::RepoRead, W::BotTokenReadRepository, F::RepoCopy(M::Confidential), deny(E::RepositoryCopyWithheld)),
    row(A::RepoRead, W::BotTokenReadRepository, F::RepoCopy(M::Unborn), deny(E::RepositoryCopyWithheld)),
    // The project read comes first: the mark never turns a refusal into
    // another answer (no oracle on the tip for who cannot read).
    row(A::RepoRead, W::Anonymous, F::RepoCopy(M::Open), deny(E::Unauthenticated)),
    row(A::RepoRead, W::NonMember, F::RepoCopy(M::Confidential), deny(E::NotFound)),
    row(A::RepoRead, W::Guest, F::RepoCopy(M::Confidential), deny(E::Forbidden)),
    row(A::RepoRead, W::AgentTokenApi, F::RepoCopy(M::Confidential), deny(E::InsufficientScope)),
    row(A::RepoRead, W::DevTokenApi, F::RepoCopy(M::Open), deny(E::InsufficientScope)),
];
