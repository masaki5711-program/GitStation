//! Widget design P2b (§3.3, §3.5): `deploy-approvals` (list) and the
//! `deploy` scope of `attention-count` (kpi) — the deployments waiting for
//! an approval the person can give now. Who can approve is the approval
//! route's decision (ADR-0038 step C: approvers, selectors, delegation),
//! taken by forge-deploy `portal` for each deployment; nothing here
//! decides it. The widget only shows: approving is done on GitStation's
//! screens (the portal has no buttons).

use axum::response::Response;
use forge_auth::PortalWidgetCtx;
use forge_core::deploy::DeployTier;
use forge_core::portal::{project_href, Tone, Word, MEMBER_PROJECTS_HREF};
use forge_core::ProjectId;
use forge_deploy::portal::PortalApproval;
use forge_errors::{ForgeError, Lang};
use forge_review::UserListCount;
use serde_json::{json, Value};

use super::{
    answer, common, envelope, invalid, kpi_body_of, pairs, single, unconfigured, widget_bounded,
    DEFAULT_LIMIT, LIMIT_RANGE, LOCALE, STALE_LIST_S, TZ,
};
use crate::app::AppState;
use crate::registry::{Dispatched, QueryParam};

pub const DEPLOY_APPROVALS: &str = "/api/portal-widgets/v1/deploy-approvals";

pub const DEPLOY_APPROVALS_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "env",
        description: "Environments, repeated: dev, stg or prod; none: every environment the person can approve in",
        integer_range: None,
    },
    QueryParam {
        name: "limit",
        description: "Items, 3..=10 (default 5)",
        integer_range: Some((3, 10)),
    },
    LOCALE,
    TZ,
];

/// `deploy-approvals`' settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeployQuery {
    /// Distinct, in the order asked; every tier when none was given.
    pub tiers: Vec<DeployTier>,
    pub limit: usize,
    pub lang: Lang,
}

pub(crate) fn deploy_query(raw: Option<&str>) -> Result<DeployQuery, ForgeError> {
    let pairs = pairs(raw);
    let lang = common(&pairs)?;
    let mut tiers = Vec::new();
    for (_, value) in pairs.iter().filter(|(k, _)| k == "env") {
        let tier = DeployTier::parse(value).ok_or_else(|| invalid("env", "unknown_value"))?;
        if !tiers.contains(&tier) {
            tiers.push(tier);
        }
    }
    if tiers.is_empty() {
        tiers = DeployTier::ALL.to_vec();
    }
    let limit = match single(&pairs, "limit")? {
        None => DEFAULT_LIMIT,
        Some(raw) => raw
            .parse::<usize>()
            .ok()
            .filter(|n| (LIMIT_RANGE.0..=LIMIT_RANGE.1).contains(n))
            .ok_or_else(|| invalid("limit", "out_of_range"))?,
    };
    Ok(DeployQuery { tiers, limit, lang })
}

/// How long it has waited: minutes under an hour, hours under two days,
/// then days.
pub(crate) fn waiting_text(secs: i64, lang: Lang) -> String {
    let minutes = secs.max(0) / 60;
    let (word, n) = if minutes < 60 {
        (Word::DeployWaitMinutes, minutes)
    } else if minutes < 48 * 60 {
        (Word::DeployWaitHours, minutes / 60)
    } else {
        (Word::DeployWaitDays, minutes / (24 * 60))
    };
    word.in_lang(lang).replace("{n}", &n.to_string())
}

/// `2026-11-26T09:00:00Z` → `2026-11-26 09:00 UTC` (times are answered in
/// UTC; anything else is shown as stored).
fn utc_minute(ts: &str) -> String {
    match (ts.get(..10), ts.get(11..16)) {
        (Some(day), Some(hm)) if ts.ends_with('Z') => format!("{day} {hm} UTC"),
        _ => ts.to_owned(),
    }
}

/// The `deploy-approvals` list body: `<path> → <env>`, the short SHA and
/// the wait (and "as a delegate"), the freeze's end when one holds now;
/// the badge says frozen (`stop`) or awaiting approval (`warn`). The item
/// opens the project, where its deployments are shown.
pub(crate) fn approvals_body(items: &[PortalApproval], total: i64, lang: Lang) -> Value {
    let items: Vec<Value> = items
        .iter()
        .map(|a| {
            let d = &a.deployment;
            let short = d.sha.get(..7).unwrap_or(&d.sha);
            let mut sub = vec![short.to_owned(), waiting_text(a.waiting_s, lang)];
            if a.as_delegate {
                sub.push(Word::DeployOnBehalf.in_lang(lang).to_owned());
            }
            let (word, tone) = match &a.frozen_until {
                Some(until) => {
                    sub.push(
                        Word::DeployFrozenUntil
                            .in_lang(lang)
                            .replace("{t}", &utc_minute(until)),
                    );
                    (Word::BadgeDeployFrozen, Tone::Stop)
                }
                None => (Word::BadgeDeployAwaiting, Tone::Warn),
            };
            json!({
                "id": format!("{}#{}", a.path, d.id),
                "label": format!("{} → {}", a.path, d.tier.as_str()),
                "sub": sub.join(" · "),
                "badge": { "label": word.in_lang(lang), "tone": tone.as_str() },
                "href": project_href(&a.path),
            })
        })
        .collect();
    json!({ "items": items, "total": total })
}

/// `GET deploy-approvals` → the `list` widget (widget design §3.5).
pub async fn deploy_approvals(state: AppState, request: Dispatched<PortalWidgetCtx>) -> Response {
    let query = deploy_query(request.raw_query());
    let w = request.into_write();
    let built = async {
        let q = query?;
        let deploy = state
            .deploy
            .as_deref()
            .ok_or_else(|| unconfigured("deploy_unconfigured"))?;
        let found = widget_bounded(
            &state,
            "portal_widget_deploy_approvals",
            deploy.portal_approvals(&w.proof, None, &q.tiers, q.limit),
        )
        .await?;
        envelope(
            "list",
            Word::TitleDeployApprovals.in_lang(q.lang),
            None,
            STALE_LIST_S,
            approvals_body(&found.items, found.count, q.lang),
        )
    }
    .await;
    answer(&w.ctx, built)
}

/// `attention-count?scope=deploy`: the title, card link and body of the
/// count of deployments the person can approve now (every environment).
pub(super) async fn deploy_count(
    state: &AppState,
    proof: &forge_auth::Authorized<PortalWidgetCtx>,
    projects: Option<&[ProjectId]>,
    lang: Lang,
) -> Result<(Word, &'static str, Value), ForgeError> {
    let deploy = state
        .deploy
        .as_deref()
        .ok_or_else(|| unconfigured("deploy_unconfigured"))?;
    let found = widget_bounded(
        state,
        "portal_widget_attention_count",
        deploy.portal_approvals(proof, projects, &DeployTier::ALL, 0),
    )
    .await?;
    let count = UserListCount {
        count: found.count,
        capped: false,
        truncated: found.truncated,
    };
    Ok((
        Word::TitleDeployApprovals,
        MEMBER_PROJECTS_HREF,
        kpi_body_of(count, Word::UnitDeployments, Tone::Warn, lang),
    ))
}

#[cfg(test)]
#[allow(clippy::expect_used)] // tests may fail loudly (rust/testing.md)
mod tests {
    use super::*;
    use forge_core::deploy::DeploymentStatus;
    use forge_deploy::StoredDeployment;

    fn rule(e: ForgeError) -> (String, String) {
        let ctx = |k: &str| {
            e.context()
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        (ctx("param"), ctx("rule"))
    }

    #[test]
    fn the_settings_default_to_every_environment_and_five_items() {
        let q = deploy_query(None).expect("defaults");
        assert_eq!(q.tiers, DeployTier::ALL.to_vec());
        assert_eq!((q.limit, q.lang), (5, Lang::En));
        let q = deploy_query(Some("env=prod&env=stg&env=prod&limit=10&locale=id")).expect("set");
        assert_eq!(q.tiers, vec![DeployTier::Prod, DeployTier::Stg]);
        assert_eq!((q.limit, q.lang), (10, Lang::Id));
        for (raw, want) in [
            ("env=production", ("env", "unknown_value")),
            ("limit=2", ("limit", "out_of_range")),
            ("limit=11", ("limit", "out_of_range")),
            ("limit=3&limit=4", ("limit", "repeated")),
            ("tz=a//b", ("tz", "invalid")),
        ] {
            let e = deploy_query(Some(raw)).expect_err(raw);
            assert_eq!(
                e.code(),
                forge_errors::ErrorCode::PortalWidgetSettingInvalid
            );
            assert_eq!(rule(e), (want.0.to_owned(), want.1.to_owned()), "{raw}");
        }
    }

    #[test]
    fn the_wait_is_minutes_then_hours_then_days() {
        assert_eq!(waiting_text(-5, Lang::En), "waiting 0 min");
        assert_eq!(waiting_text(59 * 60 + 59, Lang::Ja), "59 分待ち");
        assert_eq!(waiting_text(3600, Lang::Ja), "1 時間待ち");
        assert_eq!(waiting_text(47 * 3600 + 3599, Lang::En), "waiting 47 h");
        assert_eq!(waiting_text(48 * 3600, Lang::Id), "menunggu 2 hari");
    }

    fn approval(frozen: Option<&str>, as_delegate: bool) -> PortalApproval {
        PortalApproval {
            path: "acme/web".to_owned(),
            deployment: StoredDeployment {
                id: 7,
                project_id: 1,
                tier: DeployTier::Prod,
                sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                status: DeploymentStatus::PendingApproval,
                requested_by: Some(2),
                merge_request_author: None,
                approved_by: None,
                approved_at: None,
                approval_expires_at: None,
                previous_sha: None,
                error_code: None,
                failed_target: None,
                created_at: "2026-11-25T08:00:00Z".to_owned(),
                started_at: None,
                finished_at: None,
                commit_subject: Some("never shown".to_owned()),
                commit_author_name: Some("never shown".to_owned()),
                commit_author_email: Some("never@example.test".to_owned()),
                change_authors: vec![3],
                approvals: 0,
            },
            frozen_until: frozen.map(str::to_owned),
            waiting_s: 90 * 60,
            as_delegate,
        }
    }

    #[test]
    fn an_item_names_the_project_environment_wait_and_freeze_only() {
        let body = approvals_body(
            &[
                approval(None, false),
                approval(Some("2026-11-26T09:30:00Z"), true),
            ],
            2,
            Lang::Ja,
        );
        assert_eq!(body["total"], 2);
        let first = &body["items"][0];
        assert_eq!(first["id"], "acme/web#7");
        assert_eq!(first["label"], "acme/web → prod");
        assert_eq!(first["sub"], "0123456 · 1 時間待ち");
        assert_eq!(first["badge"]["tone"], "warn");
        assert_eq!(first["badge"]["label"], "承認待ち");
        assert_eq!(first["href"], "/acme/web");
        let second = &body["items"][1];
        assert_eq!(
            second["sub"],
            "0123456 · 1 時間待ち · 代理で承認 · 凍結中（2026-11-26 09:30 UTC まで）"
        );
        assert_eq!(second["badge"]["tone"], "stop");
        let text = body.to_string();
        assert!(
            !text.contains("never"),
            "no commit subject or person: {text}"
        );
    }
}
