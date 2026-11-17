//! The PFS portal's home widgets and news (ADR-0036; the PFS portal
//! contract v1, `docs/portal/widgets.md`). Pure: the authentication and
//! service-token seams (forge-web and forge-integ do not depend on
//! forge-oidc's internals or forge-db), the lamps and tones, the words in
//! ja / en / id, the app-relative links and the news topics.
//!
//! Nothing here holds personal data: a news item names a project, a merge
//! request number, a version or an environment — the audience is staff ids
//! only, resolved when it is sent.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use forge_errors::{ForgeError, Lang};

use crate::{RequestId, UserId, UserKind};

/// Every widget route lives below this prefix (contract §5.2 `endpoint`).
pub const WIDGET_PREFIX: &str = "/api/portal-widgets/v1/";

/// The envelope's `v` (contract v1 §6.3).
pub const CONTRACT_VERSION: u32 = 1;

// ------------------------------------------------------------ authentication

/// Who a verified portal access token stands for (ADR-0036 §2): a GitStation
/// user found by the sign-in's link rules, read-only. The token's party and
/// role are facts for `authorize()`, never decided here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalIdentity {
    pub user_id: UserId,
    /// The user's account is active (`authorize()` refuses a disabled one).
    pub active: bool,
    pub kind: UserKind,
    /// The token's `azp` is `portal.client_id`.
    pub portal_party: bool,
    /// `resource_access.<auth.oidc.client_id>.roles` holds
    /// `auth.oidc.access_role`.
    pub access_role: bool,
}

/// The future a [`PortalAuthenticator`] returns.
pub type PortalAuthFuture<'a> =
    Pin<Box<dyn Future<Output = Result<PortalIdentity, ForgeError>> + Send + 'a>>;

/// Verifies the portal's Bearer token on the widget routes (forge-oidc
/// implements it; forge-web calls it through this trait, like
/// [`crate::Authenticator`]).
pub trait PortalAuthenticator: Send + Sync {
    /// `presented` is the whole token (a secret: never logged or quoted).
    ///
    /// Errors: `FG-AUTH-0026` (the token, with `reason`), `FG-AUTH-0027`
    /// (no linked user, with `reason`), `FG-AUTH-0015` (the identity
    /// provider cannot be reached), `FG-DB-0001`.
    fn authenticate_portal<'a>(
        &'a self,
        request_id: &'a RequestId,
        presented: &'a str,
    ) -> PortalAuthFuture<'a>;
}

/// A client-credentials access token (ADR-0036 §4). No `Debug` of the
/// value: it is a bearer secret.
#[derive(Clone, PartialEq, Eq)]
pub struct ServiceToken(String);

impl ServiceToken {
    pub fn new(token: String) -> Self {
        Self(token)
    }

    /// The token, for the `Authorization` header only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ServiceToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ServiceToken([redacted])")
    }
}

/// The future a [`ServiceTokenSource`] returns.
pub type ServiceTokenFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ServiceToken, ForgeError>> + Send + 'a>>;

/// Hands out the gitstation client's client-credentials token, cached
/// until shortly before it expires (forge-oidc implements it; forge-integ's
/// news sender calls it).
pub trait ServiceTokenSource: Send + Sync {
    /// Errors: `FG-INT-0024` with `reason`.
    fn service_token(&self) -> ServiceTokenFuture<'_>;
    /// Drop the cached token (the portal refused it with 401 / 403).
    fn forget(&self);
}

// ------------------------------------------------------------ lamps and tones

/// A `status` item's state (contract §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lamp {
    Ok,
    Warn,
    Stop,
    Info,
    Unknown,
}

impl Lamp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Stop => "stop",
            Self::Info => "info",
            Self::Unknown => "unknown",
        }
    }
}

/// A badge's or a delta's tone (contract §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tone {
    Ok,
    Warn,
    Stop,
    Info,
    Neutral,
}

impl Tone {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Stop => "stop",
            Self::Info => "info",
            Self::Neutral => "neutral",
        }
    }
}

/// A pipeline's stored status (`ci_pipelines.status`) as a lamp; `None`
/// (no pipeline yet) and an unknown word are `unknown`.
pub fn pipeline_lamp(status: Option<&str>) -> Lamp {
    match status {
        Some("succeeded") => Lamp::Ok,
        Some("failed" | "invalid" | "error") => Lamp::Stop,
        Some("canceled") => Lamp::Warn,
        Some("pending" | "running" | "skipped") => Lamp::Info,
        Some(_) | None => Lamp::Unknown,
    }
}

/// A deployment's stored status (`deployments.status`) as a lamp.
pub fn deploy_lamp(status: Option<&str>) -> Lamp {
    match status {
        Some("succeeded") => Lamp::Ok,
        Some("failed" | "rollback_failed") => Lamp::Stop,
        Some("rolled_back" | "blocked") => Lamp::Warn,
        Some("pending_approval" | "queued" | "running") => Lamp::Info,
        Some(_) | None => Lamp::Unknown,
    }
}

// ------------------------------------------------------------ words

/// The words the widgets and the news use, in the three languages of the
/// portal (contract §6.3: the requested language, fallback `en` — every
/// word has all three, tested).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Word {
    TitleReview,
    TitleMine,
    TitleStopped,
    TitleSpec,
    TitlePipelines,
    TitleDeploys,
    TitleStoppedMine,
    TitleStoppedReviews,
    BadgeReview,
    BadgeDraft,
    BadgeConflict,
    BadgeChecksFailed,
    BadgeOpen,
    BadgeStopped,
    BadgeSpec,
    LampOk,
    LampWarn,
    LampStop,
    LampInfo,
    LampUnknown,
    ReferenceStandard,
    UnitChanges,
    DeltaAtStandard,
    DeltaAboveStandard,
}

impl Word {
    pub const ALL: [Word; 24] = [
        Self::TitleReview,
        Self::TitleMine,
        Self::TitleStopped,
        Self::TitleSpec,
        Self::TitlePipelines,
        Self::TitleDeploys,
        Self::TitleStoppedMine,
        Self::TitleStoppedReviews,
        Self::BadgeReview,
        Self::BadgeDraft,
        Self::BadgeConflict,
        Self::BadgeChecksFailed,
        Self::BadgeOpen,
        Self::BadgeStopped,
        Self::BadgeSpec,
        Self::LampOk,
        Self::LampWarn,
        Self::LampStop,
        Self::LampInfo,
        Self::LampUnknown,
        Self::ReferenceStandard,
        Self::UnitChanges,
        Self::DeltaAtStandard,
        Self::DeltaAboveStandard,
    ];

    /// `[ja, en, id]`.
    fn all(self) -> [&'static str; 3] {
        match self {
            Self::TitleReview => [
                "自分のレビュー待ち",
                "Waiting for my review",
                "Menunggu tinjauan saya",
            ],
            Self::TitleMine => [
                "自分が出した変更",
                "My open changes",
                "Perubahan saya yang terbuka",
            ],
            Self::TitleStopped => [
                "止まった自動マージ",
                "Stopped auto-merges",
                "Auto-merge yang berhenti",
            ],
            Self::TitleSpec => [
                "承認待ちの仕様書",
                "Specs awaiting my approval",
                "Spesifikasi menunggu persetujuan saya",
            ],
            Self::TitlePipelines => [
                "main のビルドとテスト",
                "Build and test of main",
                "Build dan uji main",
            ],
            Self::TitleDeploys => ["最後の配備", "Last deploy", "Deploy terakhir"],
            Self::TitleStoppedMine => [
                "止まっている自分の変更",
                "My stopped changes",
                "Perubahan saya yang berhenti",
            ],
            Self::TitleStoppedReviews => [
                "止まっている変更（レビュー分を含む）",
                "Stopped changes, including my reviews",
                "Perubahan yang berhenti, termasuk tinjauan saya",
            ],
            Self::BadgeReview => ["レビュー待ち", "Review", "Tinjau"],
            Self::BadgeDraft => ["下書き", "Draft", "Draf"],
            Self::BadgeConflict => ["衝突", "Conflict", "Konflik"],
            Self::BadgeChecksFailed => ["検査が失敗", "Checks failed", "Pemeriksaan gagal"],
            Self::BadgeOpen => ["進行中", "Open", "Terbuka"],
            Self::BadgeStopped => ["停止", "Stopped", "Berhenti"],
            Self::BadgeSpec => ["承認待ち", "Approval", "Persetujuan"],
            Self::LampOk => ["正常", "OK", "Normal"],
            Self::LampWarn => ["要確認", "Check", "Periksa"],
            Self::LampStop => ["停止", "Stopped", "Berhenti"],
            Self::LampInfo => ["実行中", "In progress", "Berjalan"],
            Self::LampUnknown => ["まだありません", "None yet", "Belum ada"],
            Self::ReferenceStandard => ["標準", "Standard", "Standar"],
            Self::UnitChanges => ["件", "changes", "perubahan"],
            Self::DeltaAtStandard => ["標準どおり", "At the standard", "Sesuai standar"],
            Self::DeltaAboveStandard => [
                "標準より {n} 件多い",
                "{n} above the standard",
                "{n} di atas standar",
            ],
        }
    }

    /// The word in `lang`.
    pub fn in_lang(self, lang: Lang) -> &'static str {
        let [ja, en, id] = self.all();
        match lang {
            Lang::Ja => ja,
            Lang::En => en,
            Lang::Id => id,
        }
    }

    /// The lamp's label.
    pub fn of_lamp(lamp: Lamp) -> Word {
        match lamp {
            Lamp::Ok => Self::LampOk,
            Lamp::Warn => Self::LampWarn,
            Lamp::Stop => Self::LampStop,
            Lamp::Info => Self::LampInfo,
            Lamp::Unknown => Self::LampUnknown,
        }
    }
}

/// The three languages, in the order the news carries them.
pub const LANGS: [Lang; 3] = [Lang::Ja, Lang::En, Lang::Id];

/// The language tag of `lang` (`ja`, `en`, `id`).
pub fn lang_tag(lang: Lang) -> &'static str {
    match lang {
        Lang::Ja => "ja",
        Lang::En => "en",
        Lang::Id => "id",
    }
}

// ------------------------------------------------------------ time

/// `secs` since the epoch as RFC 3339 in UTC (`2026-11-17T05:03:00Z`):
/// a tz-aware timestamp the portal shows in the viewer's zone.
pub fn rfc3339_utc(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(i64::MAX / 2);
    let rest = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// An IANA zone name as the contract's `tz` (`Asia/Jakarta`, `UTC`): the
/// shape only (forge keeps no zone database; times are answered in UTC,
/// which is tz-aware).
pub fn valid_tz(tz: &str) -> bool {
    (1..=64).contains(&tz.len())
        && tz.split('/').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+'))
        })
        && tz.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
}

// ------------------------------------------------------------ links

/// A merge request's screen (`/{path}/-/pull/{iid}`).
pub fn merge_request_href(project_path: &str, iid: i64) -> String {
    format!("/{project_path}/-/pull/{iid}")
}

/// A project's pipelines (`/{path}/-/pipelines`).
pub fn pipelines_href(project_path: &str) -> String {
    format!("/{project_path}/-/pipelines")
}

/// A project's home (`/{path}`): its deployments are shown there.
pub fn project_href(project_path: &str) -> String {
    format!("/{project_path}")
}

/// A project's tags (`/{path}/-/tags`): the released spec versions.
pub fn tags_href(project_path: &str) -> String {
    format!("/{project_path}/-/tags")
}

/// The caller's own merge requests (`/-/merge-requests`).
pub const MY_MERGE_REQUESTS_HREF: &str = "/-/merge-requests";

// ------------------------------------------------------------ news

/// What happened (ADR-0036 §4). The stored word is `as_str`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NewsTopic {
    /// A deployment ended failed, rolled back or with a failed rollback.
    DeployFailed,
    /// A deployment succeeded.
    DeployFinished,
    /// Auto-merge stopped on a merge request (one of the four stop classes).
    AutoMergeStopped,
    /// A version of a specification was released (`spec.released`).
    SpecReleased,
}

impl NewsTopic {
    pub const ALL: [NewsTopic; 4] = [
        Self::DeployFailed,
        Self::DeployFinished,
        Self::AutoMergeStopped,
        Self::SpecReleased,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeployFailed => "deploy_failed",
            Self::DeployFinished => "deploy_finished",
            Self::AutoMergeStopped => "auto_merge_stopped",
            Self::SpecReleased => "spec_released",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == raw)
    }

    /// The contract's `kind` (§7.1).
    pub fn kind(self) -> &'static str {
        match self {
            Self::DeployFailed | Self::AutoMergeStopped => "alert",
            Self::DeployFinished => "notice",
            Self::SpecReleased => "release",
        }
    }

    /// The contract's `severity`.
    pub fn severity(self) -> Tone {
        match self {
            Self::DeployFailed => Tone::Stop,
            Self::AutoMergeStopped => Tone::Warn,
            Self::DeployFinished => Tone::Ok,
            Self::SpecReleased => Tone::Info,
        }
    }

    /// How long the portal keeps showing it (`expires_at` = `occurred_at`
    /// + this many days).
    pub fn days_shown(self) -> i64 {
        match self {
            Self::DeployFailed | Self::AutoMergeStopped | Self::DeployFinished => 7,
            Self::SpecReleased => 30,
        }
    }
}

/// The facts a news item is written from (no personal data: ADR-0036 §4).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewsFacts {
    /// The project's path when the news is sent.
    pub project_path: String,
    /// A merge request's number.
    pub iid: Option<i64>,
    /// The environment (`stg` / `prod`).
    pub tier: Option<String>,
    /// The released version (`v1.2.0`).
    pub version: Option<String>,
    /// The deployed commit, 12 characters.
    pub sha_short: Option<String>,
    /// The deployment's end (`failed`, `rolled_back`, `rollback_failed`,
    /// `succeeded`).
    pub status: Option<String>,
}

/// The news item's title in `lang`.
pub fn news_title(topic: NewsTopic, f: &NewsFacts, lang: Lang) -> String {
    let p = f.project_path.as_str();
    let tier = f.tier.as_deref().unwrap_or("-");
    let iid = f.iid.unwrap_or_default();
    let version = f.version.as_deref().unwrap_or("-");
    match (topic, lang) {
        (NewsTopic::DeployFailed, Lang::Ja) => format!("{p} の {tier} への配備が失敗しました"),
        (NewsTopic::DeployFailed, Lang::En) => format!("Deploy of {p} to {tier} failed"),
        (NewsTopic::DeployFailed, Lang::Id) => format!("Deploy {p} ke {tier} gagal"),
        (NewsTopic::DeployFinished, Lang::Ja) => format!("{p} を {tier} に配備しました"),
        (NewsTopic::DeployFinished, Lang::En) => format!("{p} was deployed to {tier}"),
        (NewsTopic::DeployFinished, Lang::Id) => format!("{p} telah di-deploy ke {tier}"),
        (NewsTopic::AutoMergeStopped, Lang::Ja) => {
            format!("{p} の変更 !{iid} の自動マージが止まりました")
        }
        (NewsTopic::AutoMergeStopped, Lang::En) => {
            format!("Auto-merge stopped on change !{iid} of {p}")
        }
        (NewsTopic::AutoMergeStopped, Lang::Id) => {
            format!("Auto-merge berhenti pada perubahan !{iid} di {p}")
        }
        (NewsTopic::SpecReleased, Lang::Ja) => format!("{p} の仕様書 {version} を公開しました"),
        (NewsTopic::SpecReleased, Lang::En) => format!("Spec {version} of {p} was released"),
        (NewsTopic::SpecReleased, Lang::Id) => format!("Spesifikasi {version} dari {p} dirilis"),
    }
}

/// The news item's body in `lang` (one line).
pub fn news_body(topic: NewsTopic, f: &NewsFacts, lang: Lang) -> String {
    let sha = f.sha_short.as_deref().unwrap_or("-");
    match (topic, lang) {
        (NewsTopic::DeployFailed, Lang::Ja) => match f.status.as_deref() {
            Some("rolled_back") => format!("コミット {sha}。前の版に戻しました。"),
            Some("rollback_failed") => format!("コミット {sha}。前の版に戻せませんでした。"),
            _ => format!("コミット {sha}。原因を GitStation で確認してください。"),
        },
        (NewsTopic::DeployFailed, Lang::En) => match f.status.as_deref() {
            Some("rolled_back") => format!("Commit {sha}. The previous version is back."),
            Some("rollback_failed") => {
                format!("Commit {sha}. The previous version could not be restored.")
            }
            _ => format!("Commit {sha}. See GitStation for the cause."),
        },
        (NewsTopic::DeployFailed, Lang::Id) => match f.status.as_deref() {
            Some("rolled_back") => format!("Commit {sha}. Versi sebelumnya dipulihkan."),
            Some("rollback_failed") => {
                format!("Commit {sha}. Versi sebelumnya tidak dapat dipulihkan.")
            }
            _ => format!("Commit {sha}. Lihat GitStation untuk penyebabnya."),
        },
        (NewsTopic::DeployFinished, Lang::Ja) => format!("コミット {sha}。"),
        (NewsTopic::DeployFinished, Lang::En | Lang::Id) => format!("Commit {sha}."),
        (NewsTopic::AutoMergeStopped, Lang::Ja) => {
            "理由と次にすることは変更の画面にあります。".to_owned()
        }
        (NewsTopic::AutoMergeStopped, Lang::En) => {
            "The change's screen says why and what to do next.".to_owned()
        }
        (NewsTopic::AutoMergeStopped, Lang::Id) => {
            "Layar perubahan menjelaskan alasannya dan langkah berikutnya.".to_owned()
        }
        (NewsTopic::SpecReleased, Lang::Ja) => format!("コミット {sha}。"),
        (NewsTopic::SpecReleased, Lang::En | Lang::Id) => format!("Commit {sha}."),
    }
}

/// The app-relative link of a news item.
pub fn news_href(topic: NewsTopic, f: &NewsFacts) -> String {
    match topic {
        NewsTopic::DeployFailed | NewsTopic::DeployFinished => project_href(&f.project_path),
        NewsTopic::AutoMergeStopped => match f.iid {
            Some(iid) => merge_request_href(&f.project_path, iid),
            None => project_href(&f.project_path),
        },
        NewsTopic::SpecReleased => tags_href(&f.project_path),
    }
}

/// The idempotency key of a deployment's end (contract §7.1 `source_id`).
pub fn deploy_source_id(deployment: i64, status: &str) -> String {
    format!("gitstation-deploy-{deployment}-{status}")
}

/// The idempotency key of an auto-merge stop on a head.
pub fn auto_merge_source_id(merge_request: i64, head_sha: &str) -> String {
    let head: String = head_sha.chars().take(12).collect();
    format!("gitstation-automerge-{merge_request}-{head}")
}

/// The idempotency key of a spec release (project, tag hash).
pub fn spec_source_id(project: i64, tag_hash: &str) -> String {
    let hash: String = tag_hash.chars().take(16).collect();
    format!("gitstation-spec-{project}-{hash}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_word_has_three_languages() {
        for word in Word::ALL {
            for lang in LANGS {
                assert!(!word.in_lang(lang).trim().is_empty(), "{word:?} {lang:?}");
            }
            assert_ne!(word.in_lang(Lang::Ja), word.in_lang(Lang::En), "{word:?}");
        }
    }

    #[test]
    fn lamps_follow_the_stored_states() {
        assert_eq!(pipeline_lamp(Some("succeeded")), Lamp::Ok);
        assert_eq!(pipeline_lamp(Some("failed")), Lamp::Stop);
        assert_eq!(pipeline_lamp(Some("error")), Lamp::Stop);
        assert_eq!(pipeline_lamp(Some("running")), Lamp::Info);
        assert_eq!(pipeline_lamp(Some("canceled")), Lamp::Warn);
        assert_eq!(pipeline_lamp(None), Lamp::Unknown);
        assert_eq!(pipeline_lamp(Some("new_word")), Lamp::Unknown);
        assert_eq!(deploy_lamp(Some("succeeded")), Lamp::Ok);
        assert_eq!(deploy_lamp(Some("rollback_failed")), Lamp::Stop);
        assert_eq!(deploy_lamp(Some("rolled_back")), Lamp::Warn);
        assert_eq!(deploy_lamp(Some("pending_approval")), Lamp::Info);
        assert_eq!(deploy_lamp(None), Lamp::Unknown);
        for status in crate::deploy::DeploymentStatus::ALL {
            assert_ne!(
                deploy_lamp(Some(status.as_str())),
                Lamp::Unknown,
                "{status:?}"
            );
        }
    }

    #[test]
    fn news_is_written_in_three_languages_without_people() {
        let facts = NewsFacts {
            project_path: "acme/app".to_owned(),
            iid: Some(12),
            tier: Some("prod".to_owned()),
            version: Some("v1.2.0".to_owned()),
            sha_short: Some("0123456789ab".to_owned()),
            status: Some("rolled_back".to_owned()),
        };
        for topic in NewsTopic::ALL {
            assert_eq!(NewsTopic::parse(topic.as_str()), Some(topic));
            for lang in LANGS {
                let title = news_title(topic, &facts, lang);
                assert!(title.contains("acme/app"), "{topic:?} {lang:?}: {title}");
                assert!(!news_body(topic, &facts, lang).is_empty());
            }
            assert!(news_href(topic, &facts).starts_with("/acme/app"));
            assert!(matches!(
                topic.kind(),
                "alert" | "approval" | "notice" | "release"
            ));
        }
        assert_eq!(
            news_href(NewsTopic::AutoMergeStopped, &facts),
            "/acme/app/-/pull/12"
        );
    }

    #[test]
    fn source_ids_are_stable_and_bounded() {
        assert_eq!(deploy_source_id(7, "failed"), "gitstation-deploy-7-failed");
        assert_eq!(
            auto_merge_source_id(9, &"a".repeat(40)),
            "gitstation-automerge-9-aaaaaaaaaaaa"
        );
        assert_eq!(
            spec_source_id(3, &"f".repeat(64)),
            "gitstation-spec-3-ffffffffffffffff"
        );
    }

    #[test]
    fn times_are_rfc3339_utc_and_zones_are_checked() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_835_395_200), "2028-02-29T00:00:00Z");
        assert_eq!(rfc3339_utc(1_794_891_780), "2026-11-17T05:03:00Z");
        for good in [
            "UTC",
            "Asia/Jakarta",
            "America/Argentina/Buenos_Aires",
            "Etc/GMT+7",
        ] {
            assert!(valid_tz(good), "{good}");
        }
        for bad in ["", "/Asia", "Asia//Tokyo", "Asia/To kyo", "../etc", "1Zone"] {
            assert!(!valid_tz(bad), "{bad}");
        }
    }

    #[test]
    fn a_service_token_never_prints() {
        let token = ServiceToken::new("eyJ.secret.sig".to_owned());
        assert!(!format!("{token:?}").contains("secret"));
        assert_eq!(token.expose(), "eyJ.secret.sig");
    }
}
