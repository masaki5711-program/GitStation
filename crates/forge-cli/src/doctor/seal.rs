//! The audit seal checks of `forge doctor` (ADR-0018 §4, step C8):
//! `audit.seal_backlog` (`FG-DB-0015`) and `audit.seal_verify`
//! (`FG-DB-0011`: the seals of the last 24 hours by `sealed_at`
//! recomputed, read-only).
//!
//! The HMACs are checked only where `audit.seal_keys_file` is configured —
//! monitoring hosts must have it (runbook audit-seal-mismatch). Without it,
//! existing seals are checked for values and chain only and the check is
//! `warn`, never `ok`; an unreadable key file is a failure. `sealed_at` is
//! not authenticated (see forge-db `seal::verify`): the full walk over every
//! seal is C9's `audit.seal_reverify`.

use forge_core::config::Config;
use forge_db::{seal, sequencer, Db};

use super::{fail, ok, skip, warn, Check, Outcome};

pub(super) async fn seal_checks(db: &Db, cfg: &Config) -> [Check; 2] {
    let backlog = match seal::backlog(db).await {
        Ok(Some(b)) => match b.check() {
            Ok(()) => ok("audit.seal_backlog", &backlog_detail(&b)),
            Err(e) => fail("audit.seal_backlog", e),
        },
        Ok(None) => skip("audit.seal_backlog", "schema not migrated"),
        Err(e) => fail("audit.seal_backlog", e),
    };
    let migrated = !matches!(backlog.outcome, Outcome::Skip(_));
    let verify = if !migrated {
        skip("audit.seal_verify", "schema not migrated")
    } else {
        match cfg.audit.seal_keys() {
            Err(e) => fail("audit.seal_verify", e),
            Ok(keys) => {
                let scope = seal::VerifyScope::SealedWithin(seal::DOCTOR_WINDOW);
                match seal::verify_range(db, keys.as_ref(), scope).await {
                    Ok(v) => verified("audit.seal_verify", &v),
                    Err(e) => fail("audit.seal_verify", e),
                }
            }
        }
    };
    [backlog, verify]
}

fn backlog_detail(b: &seal::SealBacklog) -> String {
    let sealed = b.last_sealed_seq.map_or_else(
        || "nothing sealed yet".to_owned(),
        |s| format!("sealed to {s}"),
    );
    format!(
        "{sealed}, {} numbered and {} unnumbered rows waiting, audit sequencing {}",
        sequencer::shown(b.unsealed_numbered),
        sequencer::shown(b.unsequenced),
        if b.audit_sequencing { "on" } else { "off" }
    )
}

/// `ok` only when everything in the window was fully checked.
fn verified(name: &'static str, v: &seal::Verified) -> Check {
    let counted = format!("{} seals, {} rows in 24 h", v.seals, v.rows);
    let unchecked_hmac = v.seals > 0 && v.hmac == seal::HmacCheck::NoKeys;
    match (unchecked_hmac, v.truncated) {
        (false, false) => {
            let hmac = match v.hmac {
                seal::HmacCheck::Checked => "HMAC checked",
                seal::HmacCheck::NoKeys => "nothing to check",
            };
            ok(name, &format!("{counted}, {hmac}"))
        }
        (true, _) => warn(
            name,
            &format!(
                "{counted}, values and chain only: HMAC not checked \
                 (audit.seal_keys_file not set on this host)"
            ),
        ),
        (false, true) => warn(
            name,
            &format!(
                "{counted}: only the oldest {} checked",
                seal::MAX_VERIFY_SEALS
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(seals: usize, hmac: seal::HmacCheck, truncated: bool) -> seal::Verified {
        seal::Verified {
            seals,
            rows: 3,
            hmac,
            truncated,
        }
    }

    fn shown(c: &Check) -> (&'static str, String) {
        match &c.outcome {
            Outcome::Ok(d) => ("ok", d.clone()),
            Outcome::Warn(d) => ("warn", d.clone()),
            Outcome::Fail(e) => ("fail", e.to_string()),
            Outcome::Skip(d) => ("skip", (*d).to_owned()),
        }
    }

    #[test]
    fn seals_without_a_checked_hmac_are_a_warning_never_ok() {
        use seal::HmacCheck::{Checked, NoKeys};
        let (status, detail) = shown(&verified("v", &v(2, NoKeys, false)));
        assert_eq!(status, "warn");
        assert!(detail.contains("HMAC not checked"), "{detail}");
        assert_eq!(shown(&verified("v", &v(0, NoKeys, false))).0, "ok");
        let (status, detail) = shown(&verified("v", &v(2, Checked, false)));
        assert_eq!(
            (status, detail.as_str()),
            ("ok", "2 seals, 3 rows in 24 h, HMAC checked")
        );
        assert_eq!(shown(&verified("v", &v(2, Checked, true))).0, "warn");
    }
}
