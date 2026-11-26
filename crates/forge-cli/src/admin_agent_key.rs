//! `forge admin agent-key register|retire|revoke` (plan 16 A5, ADR-0032
//! A3 and A5 amendments, runbook agent-signing-key): the operator manages
//! the agent marketplace's signing keys — never over HTTP.
//!
//! A mutating operator command and, in production, an approved runbook
//! step. It connects as `forge_maint` (`database.maint_url` /
//! `database.maint_password_file`, ADR-0018 §1) and calls
//! `forge.agent_signing_key_manage` (owned by forge_seq; forge_app cannot),
//! which writes the key's audit row (`agent_signing_key.registered` /
//! `.retired` / `.revoked`).
//!
//! `register --kid N` reads `agent.signing_keys_file` for kid N's public
//! key (the seed never leaves forge-core): register a kid **before** it is
//! the file's highest (the one that signs). `retire --kid N` stops it
//! signing (it still verifies what it signed); `revoke --kid N --reason`
//! refuses every signature it made (runbook agent-supply-chain).

use forge_core::config::Config;
use forge_core::RequestId;
use forge_db::agent_marketplace::{manage_signing_key, KeyAction};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

use crate::runtime::block_on;

/// What the operator asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyCommand {
    Register { kid: u32 },
    Retire { kid: u32 },
    Revoke { kid: u32, reason: String },
}

fn not_in_file(kid: u32) -> ForgeError {
    forge_core::agent_signing::key_file_invalid("key_not_in_file").with_context("kid", kid)
}

/// Run `command`; the line it prints.
pub fn run(
    cfg: &Config,
    command: &KeyCommand,
    request_id: &RequestId,
) -> Result<String, ForgeError> {
    let (kid, line) = match command {
        KeyCommand::Register { kid } => {
            let file = cfg
                .agent
                .signing_keys()?
                .ok_or_else(|| forge_core::agent_signing::key_file_invalid("not_configured"))?;
            let key = file
                .keys()
                .find(|k| k.kid == *kid)
                .ok_or_else(|| not_in_file(*kid))?;
            (*kid, Some(key.public_key.clone())) // clone: the call borrows it
        }
        KeyCommand::Retire { kid } | KeyCommand::Revoke { kid, .. } => (*kid, None),
    };
    if let KeyCommand::Revoke { reason, .. } = command {
        let n = reason.trim().chars().count();
        if !(1..=500).contains(&n) {
            return Err(ForgeError::new(ErrorCode::RequestInvalid)
                .with_context("param", "reason")
                .with_context("reason", "length"));
        }
    }
    let kid_text = kid.to_string();
    let action = match (command, line.as_deref()) {
        (KeyCommand::Register { .. }, Some(public_key)) => KeyAction::Register { public_key },
        (KeyCommand::Retire { .. }, _) => KeyAction::Retire,
        (KeyCommand::Revoke { reason, .. }, _) => KeyAction::Revoke {
            reason: reason.trim(),
        },
        (KeyCommand::Register { .. }, None) => {
            return Err(ForgeError::new(ErrorCode::Internal).with_context("reason", "no_public_key"))
        }
    };
    let verb = match action {
        KeyAction::Register { .. } => "registered",
        KeyAction::Retire => "retired",
        KeyAction::Revoke { .. } => "revoked",
    };
    block_on(async {
        let mut maint = Db::connect_maint(&cfg.database).await?;
        let done = manage_signing_key(&mut maint, &kid_text, action, request_id).await;
        maint.close().await;
        let row = done?;
        tracing::warn!(
            target: "forge_cli",
            event = "forge.agent.signing_key_changed",
            request_id = %request_id,
            kid,
            action = verb,
            key_row_id = row,
            "agent signing key {verb}"
        );
        Ok(format!("agent signing key {kid} {verb}\n"))
    })
}
