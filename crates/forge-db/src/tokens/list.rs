//! `tokens::list`: the owner's tokens on the read pool (which cannot see
//! hashes), never the secret.

use forge_core::ids::UserId;
use forge_errors::ForgeError;

use super::{parse_scopes, TokenInfo};
use crate::{read_query_error, Db};

type ListRow = (
    i64,
    String,
    String,
    Vec<String>,
    String,
    String,
    Option<String>,
    Option<String>,
);

/// The owner's tokens, newest first (revoked and expired ones included).
/// Reads on the read pool, which cannot see the hashes.
pub async fn list(db: &Db, user: UserId) -> Result<Vec<TokenInfo>, ForgeError> {
    let rows: Vec<ListRow> = db
        .within("tokens.list", async {
            let mut conn = db.acquire().await?;
            sqlx::query_as(concat!(
                "SELECT id, public_id, name, scopes, ",
                utc!("created_at"),
                ", ",
                utc!("expires_at"),
                ", ",
                utc!("revoked_at"),
                ", ",
                utc!("last_used_at"),
                " FROM forge.tokens WHERE user_id = $1 ORDER BY created_at DESC, id DESC"
            ))
            .bind(user.get())
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| read_query_error("tokens.list", e))
        })
        .await?;
    rows.into_iter()
        .map(
            |(
                token_id,
                public_id,
                name,
                scopes,
                created_at,
                expires_at,
                revoked_at,
                last_used_at,
            )| {
                let scopes = parse_scopes(token_id, &public_id, &scopes)?;
                Ok(TokenInfo {
                    token_id,
                    public_id,
                    name,
                    scopes,
                    created_at,
                    expires_at,
                    revoked_at,
                    last_used_at,
                })
            },
        )
        .collect()
}
