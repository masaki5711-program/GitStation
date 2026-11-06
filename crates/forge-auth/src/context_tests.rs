//! The context API (M3 X5, plan 03 §6, ADR-0023 Amendment X5): `ContextRead`
//! is decided by `RepoBrowse`'s rule (every row a twin), and who counts as
//! an AI agent for the confidential boundary of plan 06 D-5.

use forge_core::UserId;

use crate::matrix::ROWS;
use crate::{AccountKind, Action, Credential, Principal, Scope};

fn caller(credential: Credential, kind: AccountKind) -> Principal {
    Principal::authenticated(UserId::new(5), false, true, credential, kind)
}

/// M3 X5: the context API is decided by the browse rule, so every
/// `RepoBrowse` row has its `ContextRead` twin (allow and deny alike).
#[test]
fn the_context_api_mirrors_every_browse_row() {
    let twins = |action: Action| -> Vec<String> {
        let mut rows: Vec<String> = ROWS
            .iter()
            .filter(|r| r.action == action)
            .map(|r| format!("{:?} {:?} {:?}", r.actor, r.fixture, r.expect))
            .collect();
        rows.sort();
        rows
    };
    let context = twins(Action::ContextRead);
    assert!(!context.is_empty(), "ContextRead has no rows");
    assert_eq!(
        context,
        twins(Action::RepoBrowse),
        "ContextRead rows must be RepoBrowse's twins"
    );
    assert_eq!(Action::ContextRead.required_scope(), Some(Scope::ReadApi));
}

/// Plan 06 D-5: an AI agent is never given what is marked confidential.
/// An agent is a credential issued with `is_agent` (ADR-0017 §4.0) or any
/// account that is no person (a service account, a bot: `users.kind`,
/// ADR-0024 §8.4); a person's session or own token is not.
#[test]
fn an_agent_token_or_a_non_person_account_is_an_agent_audience() {
    let token = || Credential::Token(vec![Scope::ReadApi]);
    assert!(!caller(Credential::Session, AccountKind::Human).is_agent_audience());
    assert!(!caller(token(), AccountKind::Human).is_agent_audience());
    assert!(caller(token(), AccountKind::Human)
        .as_agent(true)
        .is_agent_audience());
    assert!(caller(Credential::Session, AccountKind::Human)
        .as_agent(true)
        .is_agent_audience());
    assert!(caller(token(), AccountKind::ServiceAccount).is_agent_audience());
    assert!(caller(token(), AccountKind::Bot).is_agent_audience());
    // Administrator rights change nothing.
    let admin = Principal::authenticated(
        UserId::new(1),
        true,
        true,
        Credential::Token(vec![Scope::ReadApi, Scope::Admin]),
        AccountKind::Human,
    );
    assert!(!admin.clone().is_agent_audience());
    assert!(admin.as_agent(true).is_agent_audience());
}
