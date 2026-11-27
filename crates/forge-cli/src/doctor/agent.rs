//! The agent marketplace's signing keys in `forge doctor` (plan 16 A3 and
//! §8, runbook agent-signing-key):
//!
//! - `agent.signing_keys` — `agent.signing_keys_file` read for its public
//!   keys (absolute, owner only on Unix, `<kid> <64 hex>` lines), and every
//!   key in it registered in `forge.agent_signing_keys` with the same
//!   public key and not revoked; the signing key (the highest kid) not
//!   retired. `FG-AGENT-0014` with the reader's `reason`
//!   (`insecure_permissions`, `keys_unreadable`, `keys_invalid`,
//!   `signing_key_invalid`) or, against the database, `key_not_registered`,
//!   `key_mismatch` (the file's key is not the registered public key: a
//!   wrong or forged file), `key_revoked` (a revoked key is still in the
//!   file) or `signing_key_retired`, each with `kid`. A skip when the file
//!   is not configured (this host signs nothing). `warn` instead of `ok`
//!   where the OS does not let forge check the file's permissions
//!   (Windows: the ACL is the operator's, ADR-0013). Never a seed.
//! - `agent.revoked_in_use` (plan 16 §8, ADR-0032 Amendment A12) — the
//!   projects whose lock at the default branch's tip still pins a revoked
//!   part version. The candidates are the projects whose finished builds
//!   selected a version revoked now (`forge.agent_builds`, at most
//!   [`REVOKED_CAP`]); each is confirmed against the lock at its tip (read
//!   only, the whole walk bounded by [`REVOKED_TIMEOUT`]). `warn` with the
//!   count, the numeric project ids (never a name, a path or a part's
//!   file) and how to update; `warn` too when some could not be looked at
//!   or the walk was cut short. A lock written by hand (no build recorded)
//!   is not a candidate: its `forge/agent-lock` check fails instead.

use std::time::Duration;

use forge_core::agent_signing::{key_file_invalid, AgentKeyFile, PERMISSIONS_CHECKED};
use forge_core::config::Config;
use forge_db::agent_marketplace::StoredSigningKey;
use forge_db::Db;
use forge_errors::ForgeError;
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_marketplace::Marketplace;
use tokio::time::Instant;

use super::{fail, ok, skip, Check, Outcome};

/// The revoked part versions still in use (plan 16 §8).
pub(super) const REVOKED_IN_USE: &str = "agent.revoked_in_use";
/// The most candidate projects one run confirms.
pub(crate) const REVOKED_CAP: i64 = 200;
/// The bound of the whole walk (every tip read together).
pub(crate) const REVOKED_TIMEOUT: Duration = Duration::from_secs(30);
/// The bound of one git command of the walk.
const REVOKED_GIT_TIMEOUT: Duration = Duration::from_secs(20);
/// The most project ids the warning names.
const NAMED_IDS: usize = 20;

pub(super) const NAME: &str = "agent.signing_keys";

/// The check's outcome for the loaded keys and the registered ones (pure:
/// the tests feed both, and whether this OS checks permissions).
fn line(
    keys: Result<Option<AgentKeyFile>, ForgeError>,
    stored: &[StoredSigningKey],
    permissions_checked: bool,
) -> Check {
    let keys =
        match keys {
            Ok(Some(keys)) => keys,
            Ok(None) => return skip(
                NAME,
                "agent.signing_keys_file is not configured (this host cannot publish agent parts)",
            ),
            Err(e) => return fail(NAME, e.with_context("runbook", "agent-signing-key")),
        };
    let signing = keys.signing_kid();
    let problem = |reason: &'static str, kid: u32| {
        fail(
            NAME,
            key_file_invalid(reason)
                .with_context("kid", kid)
                .with_context("runbook", "agent-signing-key"),
        )
    };
    let mut count = 0usize;
    for key in keys.keys() {
        count += 1;
        let kid = key.kid.to_string();
        let Some(row) = stored.iter().find(|s| s.kid == kid) else {
            return problem("key_not_registered", key.kid);
        };
        if row.public_key != key.public_key {
            return problem("key_mismatch", key.kid);
        }
        if row.revoked {
            return problem("key_revoked", key.kid);
        }
        if key.kid == signing && row.retired {
            return problem("signing_key_retired", key.kid);
        }
    }
    let detail = format!(
        "loaded; new versions are signed with kid {signing}; {count} key(s) in the file, every one registered"
    );
    if permissions_checked {
        ok(NAME, &detail)
    } else {
        Check {
            name: NAME,
            outcome: Outcome::Warn(format!(
                "{detail}; the file's permissions are not checked on this OS (restrict its ACL to the forge service account)"
            )),
        }
    }
}

/// The check.
pub(super) async fn signing_keys_check(db: &Db, cfg: &Config) -> Check {
    match forge_db::agent_marketplace::signing_keys(db).await {
        Ok(stored) => line(cfg.agent.signing_keys(), &stored, PERMISSIONS_CHECKED),
        Err(e) => fail(NAME, e),
    }
}

/// What the walk of `agent.revoked_in_use` found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct RevokedWalk {
    /// Candidate projects looked at.
    pub(crate) checked: usize,
    /// Projects whose tip's lock pins a revoked version (ids, ascending).
    pub(crate) in_use: Vec<i64>,
    /// Candidates whose tip could not be read.
    pub(crate) unchecked: usize,
    /// The first such failure's code (no message: it may name a path).
    pub(crate) first_error: Option<&'static str>,
    /// More candidates than the cap exist.
    pub(crate) truncated: bool,
    /// The walk ran out of its time.
    pub(crate) timed_out: bool,
}

/// Confirm the candidates (module docs) against their tips.
pub(crate) async fn revoked_walk(
    db: &Db,
    market: &Marketplace,
    cap: i64,
    timeout: Duration,
) -> Result<RevokedWalk, ForgeError> {
    let deadline = Instant::now() + timeout;
    let cap = cap.max(1);
    let mut candidates =
        forge_db::agent_builder::projects_with_revoked_builds(db, cap.saturating_add(1)).await?;
    let mut walk = RevokedWalk::default();
    let keep = usize::try_from(cap).unwrap_or(usize::MAX);
    if candidates.len() > keep {
        candidates.truncate(keep);
        walk.truncated = true;
    }
    for (project, repo) in candidates {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            walk.timed_out = true;
            break;
        }
        match tokio::time::timeout(left, market.revoked_at_tip(repo)).await {
            // The tip read may still finish; nothing waits for it.
            Err(_) => {
                walk.timed_out = true;
                break;
            }
            Ok(Ok(Some(revoked))) if !revoked.is_empty() => walk.in_use.push(project.get()),
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                walk.unchecked += 1;
                walk.first_error.get_or_insert(e.code().id());
            }
        }
        walk.checked += 1;
    }
    walk.in_use.sort_unstable();
    Ok(walk)
}

/// The check's outcome for a walk (pure).
pub(super) fn revoked_line(walk: &RevokedWalk) -> Check {
    let mut gaps = Vec::new();
    if walk.unchecked > 0 {
        gaps.push(format!(
            "{} could not be read (first: {})",
            walk.unchecked,
            walk.first_error.unwrap_or("unknown")
        ));
    }
    if walk.truncated {
        gaps.push(format!("more candidates exist (cut at {REVOKED_CAP})"));
    }
    if walk.timed_out {
        gaps.push(format!(
            "the walk stopped after {} s",
            REVOKED_TIMEOUT.as_secs()
        ));
    }
    let gaps = if gaps.is_empty() {
        String::new()
    } else {
        format!("; {}", gaps.join("; "))
    };
    if !walk.in_use.is_empty() {
        let named: Vec<String> = walk
            .in_use
            .iter()
            .take(NAMED_IDS)
            .map(ToString::to_string)
            .collect();
        let more = if walk.in_use.len() > NAMED_IDS {
            ", …"
        } else {
            ""
        };
        return Check {
            name: REVOKED_IN_USE,
            outcome: Outcome::Warn(format!(
                "{} project(s) still use a revoked agent part version at the default branch's tip \
                 (project ids {}{more}); update each: project settings → Agent → \
                 \"Create the update merge request\", or `gstn agent update --project <id>` \
                 (runbook agent-supply-chain){gaps}",
                walk.in_use.len(),
                named.join(", "),
            )),
        };
    }
    let detail = if walk.checked == 0 && gaps.is_empty() {
        "no finished build selected a revoked part version".to_owned()
    } else {
        format!(
            "no lock at a default branch's tip uses a revoked part version ({} candidate(s) checked{gaps})",
            walk.checked
        )
    };
    if gaps.is_empty() {
        ok(REVOKED_IN_USE, &detail)
    } else {
        Check {
            name: REVOKED_IN_USE,
            outcome: Outcome::Warn(detail),
        }
    }
}

/// The check. `root`: the opened storage root (`None`: skipped, the
/// `storage` check fails).
pub(super) async fn revoked_in_use_check(
    db: &Db,
    cfg: &Config,
    root: Option<&StorageRoot>,
) -> Check {
    let Some(root) = root else {
        return skip(REVOKED_IN_USE, "storage root unavailable");
    };
    let mut git = GitConfig::new("git", root.path());
    git.timeout_total = REVOKED_GIT_TIMEOUT;
    let browse = Browser::new(
        Git::new(git),
        root.clone(), // clone: the browser owns its root
        BrowseLimits::default(),
    );
    let market = Marketplace::new(
        db.clone(), // clone: the pool handle is shared
        browse,
        cfg.agent.clone(), // clone: the marketplace owns its settings
    );
    match revoked_walk(db, &market, REVOKED_CAP, REVOKED_TIMEOUT).await {
        Ok(walk) => revoked_line(&walk),
        Err(e) => fail(REVOKED_IN_USE, e),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // tests may fail loudly

    use forge_errors::ErrorCode;

    use super::*;

    const SEED_1: &str = "0101010101010101010101010101010101010101010101010101010101010101";
    const SEED_2: &str = "0202020202020202020202020202020202020202020202020202020202020202";

    fn keys(text: &str) -> Result<Option<AgentKeyFile>, ForgeError> {
        AgentKeyFile::parse(text).map(Some)
    }

    fn stored(text: &str) -> Vec<StoredSigningKey> {
        AgentKeyFile::parse(text)
            .expect("keys")
            .keys()
            .map(|k| StoredSigningKey {
                kid: k.kid.to_string(),
                public_key: k.public_key.clone(),
                retired: false,
                revoked: false,
            })
            .collect()
    }

    fn reason(c: &Check) -> String {
        let Outcome::Fail(e) = &c.outcome else {
            panic!("expected a failure");
        };
        assert_eq!(e.code(), ErrorCode::SigningKeyFileInvalid);
        e.context()
            .iter()
            .find(|(k, _)| *k == "reason")
            .map(|(_, v)| v.clone())
            .expect("reason")
    }

    #[test]
    fn the_key_file_must_match_the_registered_keys() {
        let both = format!("1 {SEED_1}\n2 {SEED_2}\n");
        assert!(matches!(
            line(Ok(None), &[], true).outcome,
            Outcome::Skip(_)
        ));
        assert!(matches!(
            line(keys(&both), &stored(&both), true).outcome,
            Outcome::Ok(_)
        ));
        // Only the older key registered: the signing key is not.
        let only_one = stored(&format!("1 {SEED_1}"));
        assert_eq!(
            reason(&line(keys(&both), &only_one, true)),
            "key_not_registered"
        );
        // The same kid with another public key.
        let other = stored(&format!("2 {SEED_2}"))
            .into_iter()
            .map(|k| StoredSigningKey {
                kid: "1".to_owned(),
                ..k
            })
            .collect::<Vec<_>>();
        assert_eq!(
            reason(&line(keys(&format!("1 {SEED_1}")), &other, true)),
            "key_mismatch"
        );
        // A revoked key still in the file; a retired signing key (the
        // signing key, kid 2, is listed first).
        let mut revoked = stored(&both);
        revoked[1].revoked = true;
        assert_eq!(reason(&line(keys(&both), &revoked, true)), "key_revoked");
        let mut retired = stored(&both);
        retired[0].retired = true;
        assert_eq!(
            reason(&line(keys(&both), &retired, true)),
            "signing_key_retired"
        );
        // A retired older key is fine (it only verifies).
        let mut older_retired = stored(&both);
        older_retired[1].retired = true;
        assert!(matches!(
            line(keys(&both), &older_retired, true).outcome,
            Outcome::Ok(_)
        ));
        // The reader's refusal is the check's failure.
        assert_eq!(reason(&line(keys(""), &[], true)), "keys_invalid");
    }

    /// Review A3: where the OS does not let forge check the file's
    /// permissions (Windows), a matching file is `warn`, never `ok`.
    #[test]
    fn unchecked_permissions_warn_instead_of_ok() {
        let one = format!("1 {SEED_1}\n");
        let Outcome::Warn(detail) = line(keys(&one), &stored(&one), false).outcome else {
            panic!("expected a warning");
        };
        assert!(detail.contains("not checked on this OS"), "{detail}");
    }

    fn warning(c: &Check) -> &str {
        let Outcome::Warn(detail) = &c.outcome else {
            panic!("expected a warning");
        };
        detail
    }

    /// Plan 16 §8: the count, the ids and how to update; never a name.
    #[test]
    fn revoked_in_use_warns_with_the_count_and_how_to_update() {
        let none = revoked_line(&RevokedWalk::default());
        assert!(matches!(none.outcome, Outcome::Ok(_)));
        let confirmed = revoked_line(&RevokedWalk {
            checked: 3,
            ..RevokedWalk::default()
        });
        assert!(matches!(confirmed.outcome, Outcome::Ok(_)));

        let used = revoked_line(&RevokedWalk {
            checked: 3,
            in_use: vec![4, 9],
            ..RevokedWalk::default()
        });
        let detail = warning(&used);
        assert!(detail.starts_with("2 project(s) still use"), "{detail}");
        assert!(detail.contains("project ids 4, 9)"), "{detail}");
        assert!(
            detail.contains("gstn agent update --project <id>"),
            "{detail}"
        );
        assert!(detail.contains("runbook agent-supply-chain"), "{detail}");

        let many = revoked_line(&RevokedWalk {
            checked: 25,
            in_use: (1..=25).collect(),
            ..RevokedWalk::default()
        });
        let detail = warning(&many);
        assert!(detail.starts_with("25 project(s)"), "{detail}");
        assert!(detail.contains("20, …"), "{detail}");
        assert!(!detail.contains("21"), "{detail}");
    }

    /// A walk that could not look at everything is never `ok`.
    #[test]
    fn an_incomplete_walk_warns() {
        let unread = RevokedWalk {
            checked: 2,
            unchecked: 1,
            first_error: Some("FG-GIT-0001"),
            ..RevokedWalk::default()
        };
        let cut = RevokedWalk {
            checked: 200,
            truncated: true,
            ..RevokedWalk::default()
        };
        let late = RevokedWalk {
            checked: 1,
            timed_out: true,
            ..RevokedWalk::default()
        };
        for walk in [&unread, &cut, &late] {
            let c = revoked_line(walk);
            assert!(warning(&c).starts_with("no lock"));
        }
        assert!(warning(&revoked_line(&unread)).contains("first: FG-GIT-0001"));
        assert!(warning(&revoked_line(&cut)).contains("cut at 200"));
        assert!(warning(&revoked_line(&late)).contains("stopped after 30 s"));
        // Found ones are named even when the walk was cut short.
        let both = revoked_line(&RevokedWalk {
            in_use: vec![7],
            ..late
        });
        assert!(warning(&both).starts_with("1 project(s)"));
        assert!(warning(&both).contains("stopped after"));
    }
}
