//! `forge admin user link-identity --user <u> --issuer <iss> --subject
//! <sub>` (ADR-0021 §4 2a): the operator links an identity — the only way
//! for an administrator, and how every `FG-AUTH-0014` is settled
//! (runbook oidc-identity-link). Like the operator's move and rename
//! commands it is an exception to the single decision point (ADR-0019 §8,
//! CLAUDE.md contract 4): the host operator is the trust root, it runs as
//! actor `system` (`actor_label = forge-cli:<OS account>`) and cannot be
//! reached from HTTP — any other actor is `FG-SYS-0001
//! reason=operator_not_system`.

use forge_core::UserId;
use forge_db::change::{Actor, ChangeContext};
use forge_db::identities::{self, OperatorLink};
use forge_db::Db;
use forge_errors::ForgeError;

use crate::error::internal;

/// Link `(issuer, subject)` to `user` as the operator.
pub async fn operator_link_identity(
    db: &Db,
    ctx: &ChangeContext,
    user: UserId,
    issuer: &str,
    subject: &str,
) -> Result<OperatorLink, ForgeError> {
    if ctx.actor != Actor::System {
        return Err(internal("operator_not_system"));
    }
    identities::link_by_operator(db, ctx, user, issuer, subject).await
}
