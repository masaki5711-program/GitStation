//! The user directory reads of M2 MR6, on the read pool (which never sees
//! e-mail, employee number or external id, tables.toml):
//!
//! - [`search`]: `GET /api/v1/users?search=` — active users whose username
//!   starts with the term or whose display name contains it, names only,
//!   at most [`MAX_SEARCH_RESULTS`] (the member picker; any signed-in user
//!   may ask, `UserSearch`).
//! - [`admin_page`]: `GET /api/v1/admin/users` — every user in id order
//!   after a keyset position, optionally of one state (administrators,
//!   `UserAdminList`).

use forge_core::ids::UserId;
use forge_core::UserKind;
use forge_errors::{ErrorCode, ForgeError};

use crate::{read_query_error, Db};

/// The fewest and most characters of a search term.
pub const MIN_SEARCH_CHARS: usize = 2;
pub const MAX_SEARCH_CHARS: usize = 64;
/// The most matches one search returns.
pub const MAX_SEARCH_RESULTS: i64 = 20;
/// The largest admin page (forge-web asks for its limit plus one).
pub const MAX_ADMIN_PAGE: u32 = 101;

/// One search match: names only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserMatch {
    pub id: UserId,
    pub username: String,
    pub display_name: String,
}

/// One user as the administration list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminUserRow {
    pub id: UserId,
    pub kind: UserKind,
    pub username: String,
    pub display_name: String,
    /// `active` or `disabled`.
    pub active: bool,
    pub is_admin: bool,
    /// RFC 3339, UTC.
    pub created_at: String,
}

/// Which users the administration list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateFilter {
    All,
    Active,
    Disabled,
}

impl StateFilter {
    fn sql(self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::Active => Some("active"),
            Self::Disabled => Some("disabled"),
        }
    }
}

fn request_invalid(rule: &'static str, len: usize) -> ForgeError {
    ForgeError::new(ErrorCode::RequestInvalid)
        .with_context("reason", "param_invalid")
        .with_context("param", "search")
        .with_context("rule", rule)
        .with_context("len", len)
}

/// `term` as a `LIKE` pattern body: `\`, `%` and `_` escaped (the
/// statement's `ESCAPE '\'`), lower-cased.
fn like_escaped(term: &str) -> String {
    let mut out = String::with_capacity(term.len());
    for c in term.to_lowercase().chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Active users matching `term` (trimmed; [`MIN_SEARCH_CHARS`] ..=
/// [`MAX_SEARCH_CHARS`] characters, no control characters — otherwise
/// `FG-WEB-0006 reason=param_invalid param=search rule=…`, nothing read).
/// An exact username first, then usernames starting with the term, then
/// display names containing it; at most [`MAX_SEARCH_RESULTS`].
pub async fn search(db: &Db, term: &str) -> Result<Vec<UserMatch>, ForgeError> {
    let term = term.trim();
    let len = term.chars().count();
    if len < MIN_SEARCH_CHARS {
        return Err(request_invalid("too_short", len));
    }
    if len > MAX_SEARCH_CHARS {
        return Err(request_invalid("too_long", len));
    }
    if term.chars().any(char::is_control) {
        return Err(request_invalid("control_character", len));
    }
    let lowered = term.to_lowercase();
    let pattern = like_escaped(term);
    let rows: Vec<(i64, String, String)> = db
        .within("users.search", async {
            let mut conn = db.acquire().await?;
            sqlx::query_as(
                "SELECT id, username, display_name FROM forge.users \
                 WHERE state = 'active' \
                   AND (username LIKE $2 || '%' ESCAPE '\\' \
                        OR lower(display_name) LIKE '%' || $2 || '%' ESCAPE '\\') \
                 ORDER BY (username = $1) DESC, (username LIKE $2 || '%' ESCAPE '\\') DESC, \
                   username \
                 LIMIT $3",
            )
            .bind(&lowered)
            .bind(&pattern)
            .bind(MAX_SEARCH_RESULTS)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| read_query_error("users.search", e))
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|(id, username, display_name)| UserMatch {
            id: UserId::new(id),
            username,
            display_name,
        })
        .collect())
}

/// Users with an id above `after`, in id order, at most `max_rows` (1 ..=
/// [`MAX_ADMIN_PAGE`], else `FG-SYS-0001 reason=page_size_out_of_range`),
/// of `state`. One statement on the read pool. A stored kind this build
/// does not know is `FG-SYS-0001 reason=unknown_user_kind`.
pub async fn admin_page(
    db: &Db,
    after: Option<UserId>,
    state: StateFilter,
    max_rows: u32,
) -> Result<Vec<AdminUserRow>, ForgeError> {
    if max_rows == 0 || max_rows > MAX_ADMIN_PAGE {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "page_size_out_of_range")
            .with_context("max_rows", max_rows));
    }
    type Row = (i64, String, String, String, String, bool, String);
    let rows: Vec<Row> = db
        .within("users.admin_page", async {
            let mut conn = db.acquire().await?;
            sqlx::query_as(
                "SELECT id, kind, username, display_name, state, is_admin, \
                   to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
                 FROM forge.users WHERE id > $1 AND ($2::text IS NULL OR state = $2) \
                 ORDER BY id LIMIT $3",
            )
            .bind(after.map_or(0, UserId::get))
            .bind(state.sql())
            .bind(i64::from(max_rows))
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| read_query_error("users.admin_page", e))
        })
        .await?;
    rows.into_iter()
        .map(
            |(id, kind, username, display_name, state, is_admin, created_at)| {
                let kind = UserKind::parse(&kind).ok_or_else(|| {
                    ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "unknown_user_kind")
                        .with_context("user_id", id)
                })?;
                Ok(AdminUserRow {
                    id: UserId::new(id),
                    kind,
                    username,
                    display_name,
                    active: state == "active",
                    is_admin,
                    created_at,
                })
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use forge_core::RequestId;

    use super::*;
    use crate::change::{Actor, ChangeContext};
    use crate::test_support::{assert_reason, FreshDb};
    use crate::users::{self, NewUser};

    #[test]
    fn like_wildcards_in_a_term_are_escaped() {
        assert_eq!(like_escaped("a_b%c\\d"), "a\\_b\\%c\\\\d");
        assert_eq!(like_escaped("Alice"), "alice");
    }

    fn ctx(request: &str) -> ChangeContext {
        ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
    }

    async fn human(t: &FreshDb, name: &str, display: &str) -> UserId {
        users::create(
            &t.db,
            &ctx(&format!("req-user-{name}")),
            &NewUser::human(name, display, &format!("{name}@example.test")),
        )
        .await
        .expect("user")
        .user_id
    }

    #[tokio::test]
    async fn search_finds_active_users_by_username_prefix_or_display_name() {
        let t = FreshDb::migrated().await;
        let al = human(&t, "al", "Synthetic Al").await;
        let alice = human(&t, "alice", "Alice Example").await;
        let malia = human(&t, "malia", "Malia Sample").await;
        let carol = human(&t, "carol", "Carol Alder").await;
        let gone = human(&t, "alfred", "Alfred Gone").await;
        let _ = users::disable(&t.db, &ctx("req-disable"), gone)
            .await
            .expect("disable");
        let found = search(&t.db, "  AL ").await.expect("search");
        let ids: Vec<UserId> = found.iter().map(|m| m.id).collect();
        // exact username first, then username prefixes, then display names
        assert_eq!(ids[0], al);
        assert_eq!(ids[1], alice);
        assert!(ids.contains(&malia) && ids.contains(&carol), "{found:?}");
        assert!(!ids.contains(&gone), "a disabled user is not offered");
        // wildcards are literal: nobody's name contains "%"
        assert!(search(&t.db, "a%").await.expect("search").is_empty());
        assert!(search(&t.db, "__").await.expect("search").is_empty());
        for (term, rule) in [
            ("a", "too_short"),
            (" ", "too_short"),
            ("a\tb", "control_character"),
        ] {
            let err = search(&t.db, term).await.expect_err(term);
            assert_reason(&err, ErrorCode::RequestInvalid, "param_invalid");
            assert!(err.to_string().contains(rule), "{term}: {err}");
        }
        let long = "x".repeat(MAX_SEARCH_CHARS + 1);
        let err = search(&t.db, &long).await.expect_err("long");
        assert!(err.to_string().contains("too_long"), "{err}");
        t.drop_now().await;
    }

    #[tokio::test]
    async fn the_admin_page_is_keyset_in_id_order_with_a_state_filter() {
        let t = FreshDb::migrated().await;
        let a = human(&t, "anna", "Anna").await;
        let b = human(&t, "bert", "Bert").await;
        let c = human(&t, "cleo", "Cleo").await;
        let _ = users::disable(&t.db, &ctx("req-disable"), b)
            .await
            .expect("disable");
        let all = admin_page(&t.db, None, StateFilter::All, 10)
            .await
            .expect("page");
        let ids: Vec<UserId> = all.iter().map(|r| r.id).collect();
        assert_eq!(ids, [a, b, c]);
        assert!(!all[1].active && all[0].active);
        assert_eq!(all[0].kind, UserKind::Human);
        let after = admin_page(&t.db, Some(a), StateFilter::All, 1)
            .await
            .expect("page");
        assert_eq!(after.iter().map(|r| r.id).collect::<Vec<_>>(), [b]);
        let active = admin_page(&t.db, None, StateFilter::Active, 10)
            .await
            .expect("page");
        assert_eq!(active.iter().map(|r| r.id).collect::<Vec<_>>(), [a, c]);
        let disabled = admin_page(&t.db, None, StateFilter::Disabled, 10)
            .await
            .expect("page");
        assert_eq!(disabled.iter().map(|r| r.id).collect::<Vec<_>>(), [b]);
        for rows in [0, MAX_ADMIN_PAGE + 1] {
            let err = admin_page(&t.db, None, StateFilter::All, rows)
                .await
                .expect_err("size");
            assert_reason(&err, ErrorCode::Internal, "page_size_out_of_range");
        }
        t.drop_now().await;
    }
}
