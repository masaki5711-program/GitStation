//! The words of a notification mail, in ja / en / id (the person's own
//! language, or the instance default). Plain text only.
//!
//! What a mail may say: the project's path, the merge request's number and
//! title, the name of the person who acted, and a link — all of which the
//! recipient can see in GitStation (their access is checked just before the
//! mail is built). Never branch names, file paths, commit ids, counts of
//! anything hidden, errors, or anything of the instance's inside.

use forge_core::NotificationKind;
use forge_errors::Lang;

use crate::mail::one_line;

/// What a mail is made from.
#[derive(Debug, Clone, Copy)]
pub struct Facts<'a> {
    pub kind: NotificationKind,
    pub lang: Lang,
    /// `group/project`, as the recipient sees it.
    pub project_path: &'a str,
    pub iid: i64,
    pub title: &'a str,
    /// The display name of the person who acted, when known.
    pub actor: Option<&'a str>,
    /// An absolute link to the merge request (needs `web.base_url`).
    pub link: Option<&'a str>,
}

/// The subject and the body.
pub fn render(facts: &Facts<'_>) -> (String, String) {
    let project = one_line(facts.project_path, 120);
    let title = one_line(facts.title, 120);
    let actor = facts
        .actor
        .map(|a| one_line(a, 60))
        .filter(|a| !a.is_empty());
    let reference = format!("{project}!{}", facts.iid);
    let subject = format!(
        "[GitStation] {reference} {}",
        headline(facts.kind, facts.lang)
    );
    let mut body = String::new();
    body.push_str(&sentence(facts.kind, facts.lang, actor.as_deref(), &title));
    body.push_str("\n\n");
    body.push_str(&format!("{}: {reference}\n", label(facts.lang)));
    if let Some(link) = facts.link {
        body.push_str(&format!("{}: {link}\n", open_label(facts.lang)));
    }
    body.push_str("\n--\n");
    body.push_str(footer(facts.lang));
    (subject, body)
}

fn headline(kind: NotificationKind, lang: Lang) -> &'static str {
    match (lang, kind) {
        (Lang::Ja, NotificationKind::MergeRequestApproved) => "が承認されました",
        (Lang::Ja, NotificationKind::MergeRequestMerged) => "がマージされました",
        (Lang::Ja, NotificationKind::MergeRequestCommented) => "にコメントがあります",
        (Lang::En, NotificationKind::MergeRequestApproved) => "was approved",
        (Lang::En, NotificationKind::MergeRequestMerged) => "was merged",
        (Lang::En, NotificationKind::MergeRequestCommented) => "has a new comment",
        (Lang::Id, NotificationKind::MergeRequestApproved) => "disetujui",
        (Lang::Id, NotificationKind::MergeRequestMerged) => "sudah di-merge",
        (Lang::Id, NotificationKind::MergeRequestCommented) => "memiliki komentar baru",
        (Lang::Ja, NotificationKind::MergeRequestConflictPredicted) => {
            "がほかのマージリクエストと衝突する見込みです"
        }
        (Lang::En, NotificationKind::MergeRequestConflictPredicted) => {
            "may conflict with another merge request"
        }
        (Lang::Id, NotificationKind::MergeRequestConflictPredicted) => {
            "kemungkinan berkonflik dengan merge request lain"
        }
    }
}

fn sentence(kind: NotificationKind, lang: Lang, actor: Option<&str>, title: &str) -> String {
    match (lang, kind, actor) {
        (Lang::Ja, NotificationKind::MergeRequestApproved, Some(a)) => {
            format!("{a} さんが、あなたのマージリクエスト「{title}」を承認しました。")
        }
        (Lang::Ja, NotificationKind::MergeRequestApproved, None) => {
            format!("あなたのマージリクエスト「{title}」が承認されました。")
        }
        (Lang::Ja, NotificationKind::MergeRequestMerged, Some(a)) => {
            format!("{a} さんが、あなたのマージリクエスト「{title}」をマージしました。")
        }
        (Lang::Ja, NotificationKind::MergeRequestMerged, None) => {
            format!("あなたのマージリクエスト「{title}」がマージされました。")
        }
        (Lang::Ja, NotificationKind::MergeRequestCommented, Some(a)) => {
            format!("{a} さんが、あなたのマージリクエスト「{title}」にコメントしました。")
        }
        (Lang::Ja, NotificationKind::MergeRequestCommented, None) => {
            format!("あなたのマージリクエスト「{title}」にコメントがありました。")
        }
        (Lang::En, NotificationKind::MergeRequestApproved, Some(a)) => {
            format!("{a} approved your merge request \"{title}\".")
        }
        (Lang::En, NotificationKind::MergeRequestApproved, None) => {
            format!("Your merge request \"{title}\" was approved.")
        }
        (Lang::En, NotificationKind::MergeRequestMerged, Some(a)) => {
            format!("{a} merged your merge request \"{title}\".")
        }
        (Lang::En, NotificationKind::MergeRequestMerged, None) => {
            format!("Your merge request \"{title}\" was merged.")
        }
        (Lang::En, NotificationKind::MergeRequestCommented, Some(a)) => {
            format!("{a} commented on your merge request \"{title}\".")
        }
        (Lang::En, NotificationKind::MergeRequestCommented, None) => {
            format!("Your merge request \"{title}\" has a new comment.")
        }
        (Lang::Id, NotificationKind::MergeRequestApproved, Some(a)) => {
            format!("{a} menyetujui permintaan merge Anda \"{title}\".")
        }
        (Lang::Id, NotificationKind::MergeRequestApproved, None) => {
            format!("Permintaan merge Anda \"{title}\" telah disetujui.")
        }
        (Lang::Id, NotificationKind::MergeRequestMerged, Some(a)) => {
            format!("{a} meng-merge permintaan merge Anda \"{title}\".")
        }
        (Lang::Id, NotificationKind::MergeRequestMerged, None) => {
            format!("Permintaan merge Anda \"{title}\" sudah di-merge.")
        }
        (Lang::Id, NotificationKind::MergeRequestCommented, Some(a)) => {
            format!("{a} berkomentar pada permintaan merge Anda \"{title}\".")
        }
        (Lang::Id, NotificationKind::MergeRequestCommented, None) => {
            format!("Permintaan merge Anda \"{title}\" memiliki komentar baru.")
        }
        // M3 X6: the scan acts, never a person (no actor); which other
        // merge request it is shows on the merge request (`gstn conflicts`).
        (Lang::Ja, NotificationKind::MergeRequestConflictPredicted, _) => {
            format!(
                "あなたのマージリクエスト「{title}」は、同じブランチに向けて開いているほかのマージリクエストと衝突する見込みです。マージリクエストの画面か `gstn conflicts` で相手と箇所を確かめてください。"
            )
        }
        (Lang::En, NotificationKind::MergeRequestConflictPredicted, _) => {
            format!(
                "Your merge request \"{title}\" is likely to conflict with another open merge request on the same branch. See the merge request or `gstn conflicts` for which one and where."
            )
        }
        (Lang::Id, NotificationKind::MergeRequestConflictPredicted, _) => {
            format!(
                "Permintaan merge Anda \"{title}\" kemungkinan berkonflik dengan merge request lain yang terbuka pada branch yang sama. Lihat merge request atau `gstn conflicts` untuk mengetahui yang mana dan di mana."
            )
        }
    }
}

fn label(lang: Lang) -> &'static str {
    match lang {
        Lang::Ja => "マージリクエスト",
        Lang::En => "Merge request",
        Lang::Id => "Permintaan merge",
    }
}

fn open_label(lang: Lang) -> &'static str {
    match lang {
        Lang::Ja => "開く",
        Lang::En => "Open",
        Lang::Id => "Buka",
    }
}

fn footer(lang: Lang) -> &'static str {
    match lang {
        Lang::Ja => {
            "このメールは GitStation から自動で送られています。返信はできません。\n\
             通知を止める・言語を変える: gstn notifications set（種類ごとに止められます）\n"
        }
        Lang::En => {
            "This e-mail was sent automatically by GitStation; replies are not read.\n\
             To stop a kind of notification or change the language: gstn notifications set\n"
        }
        Lang::Id => {
            "E-mail ini dikirim otomatis oleh GitStation; balasan tidak dibaca.\n\
             Untuk menghentikan satu jenis notifikasi atau mengganti bahasa: gstn notifications set\n"
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)] // tests may fail loudly
mod tests {
    use super::*;

    fn facts(kind: NotificationKind, lang: Lang) -> Facts<'static> {
        Facts {
            kind,
            lang,
            project_path: "team/web",
            iid: 7,
            title: "Fix rounding",
            actor: Some("Bob"),
            link: Some("https://git.example.test/team/web/-/pull/7"),
        }
    }

    #[test]
    fn every_kind_and_language_has_a_subject_and_a_body_with_the_facts() {
        for kind in NotificationKind::ALL {
            for lang in [Lang::Ja, Lang::En, Lang::Id] {
                let (subject, body) = render(&facts(*kind, lang));
                assert!(subject.starts_with("[GitStation] team/web!7 "), "{subject}");
                assert!(
                    subject.len() > "[GitStation] team/web!7 ".len(),
                    "{subject}"
                );
                assert!(!subject.contains('\n') && !subject.contains('\r'));
                assert!(body.contains("Fix rounding"), "{lang:?} {kind:?}: {body}");
                if *kind == NotificationKind::MergeRequestConflictPredicted {
                    // The scan acted, never a person: no name in the mail.
                    assert!(!body.contains("Bob"), "{lang:?} {kind:?}: {body}");
                    assert!(body.contains("gstn conflicts"), "where to look: {body}");
                } else {
                    assert!(body.contains("Bob"), "{lang:?} {kind:?}: {body}");
                }
                assert!(body.contains("team/web!7"));
                assert!(body.contains("https://git.example.test/team/web/-/pull/7"));
                assert!(body.contains("gstn notifications set"), "how to opt out");
                assert!(
                    !body.contains('{') && !body.contains('}'),
                    "unfilled: {body}"
                );
            }
        }
    }

    #[test]
    fn the_three_languages_differ_and_use_the_right_script() {
        let kind = NotificationKind::MergeRequestMerged;
        let ja = render(&facts(kind, Lang::Ja));
        let en = render(&facts(kind, Lang::En));
        let id = render(&facts(kind, Lang::Id));
        assert!(ja.0.contains("マージされました"));
        assert!(en.0.contains("was merged"));
        assert!(id.0.contains("sudah di-merge"));
        assert_ne!(ja, en);
        assert_ne!(en, id);
    }

    #[test]
    fn without_an_actor_or_a_link_the_mail_still_reads() {
        let mut f = facts(NotificationKind::MergeRequestApproved, Lang::En);
        f.actor = None;
        f.link = None;
        let (_, body) = render(&f);
        assert!(body.starts_with("Your merge request \"Fix rounding\" was approved."));
        assert!(!body.contains("Open:"));
    }

    #[test]
    fn a_title_cannot_break_the_mail_out_of_its_line() {
        let mut f = facts(NotificationKind::MergeRequestCommented, Lang::En);
        f.title = "x\r\nBcc: evil@example.test\n\nclick here";
        f.actor = Some("Eve\r\nTo: x@y.test");
        let (subject, body) = render(&f);
        assert!(!subject.contains('\n'));
        let first = body.lines().next().expect("first line");
        assert!(
            first.contains("Bcc: evil@example.test click here"),
            "{first}"
        );
        assert!(first.contains("Eve To: x@y.test"), "{first}");
    }

    #[test]
    fn a_long_title_is_cut() {
        let mut f = facts(NotificationKind::MergeRequestMerged, Lang::En);
        let long = "a".repeat(500);
        f.title = &long;
        let (_, body) = render(&f);
        assert!(body.lines().next().expect("line").chars().count() < 200);
    }

    #[test]
    fn nothing_internal_is_in_any_mail() {
        // The inputs are the only source of words: none of these appear.
        for kind in NotificationKind::ALL {
            for lang in [Lang::Ja, Lang::En, Lang::Id] {
                let (subject, body) = render(&facts(*kind, lang));
                let all = format!("{subject}\n{body}").to_lowercase();
                for forbidden in [
                    "refs/",
                    "select ",
                    "insert ",
                    "panic",
                    "stack",
                    "fg-",
                    "sqlx",
                    "/var/",
                    "c:\\",
                    "request_id",
                ] {
                    assert!(!all.contains(forbidden), "{forbidden} in {all}");
                }
            }
        }
    }
}
