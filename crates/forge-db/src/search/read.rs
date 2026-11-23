//! The ranked candidates of a query (plan 20 K3, ADR-0040): one statement
//! on the read pool under its own statement timeout. It decides nothing:
//! the filters only narrow (kinds, one project, no confidential rows for
//! an AI audience) and forge-web decides every returned row with
//! `authorize()` before it shows anything of it.

use std::time::Duration;

use forge_core::search::{CandidateFilter, SearchCandidate, SourceKind};
use forge_core::ProjectId;
use forge_errors::{ErrorCode, ForgeError};

use crate::{read_query_error, Db};

/// The search statement's own timeout (plan 20 §3.4: 2 s).
pub const SEARCH_STATEMENT_TIMEOUT: Duration = Duration::from_secs(2);
/// The most candidates one call reads.
pub const MAX_CANDIDATES: u32 = 1_000;
/// The deepest offset a cursor may reach (a search is not a full export).
pub const MAX_OFFSET: u32 = 10_000;

/// [`SEARCH_STATEMENT_TIMEOUT`] for the transaction (a test pins the two
/// together).
pub(crate) const SET_TIMEOUT_SQL: &str = "SET LOCAL statement_timeout = 2000";

const CANDIDATES_SQL: &str = "SELECT e.source_kind, e.project_id, e.source_key, e.title, \
       e.body_text, e.classification, e.truncated, \
       to_char(e.source_updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
         AS source_updated_at \
     FROM forge.search_entries e \
     WHERE e.tokens @@ $1::tsquery \
       AND (cardinality($2::text[]) = 0 OR e.source_kind = ANY ($2::text[])) \
       AND ($3::bigint IS NULL OR e.project_id = $3) \
       AND (NOT $4 OR e.classification <> 'confidential') \
     ORDER BY ts_rank_cd(e.tokens, $1::tsquery) DESC, e.source_updated_at DESC, e.id DESC \
     OFFSET $5 LIMIT $6";

#[derive(sqlx::FromRow)]
struct Row {
    source_kind: String,
    project_id: i64,
    source_key: String,
    title: String,
    body_text: String,
    classification: String,
    truncated: bool,
    source_updated_at: String,
}

/// A stored kind this build does not know (K4's `document` written by a
/// newer build) is left out, never guessed.
fn candidate(row: Row) -> Option<SearchCandidate> {
    let kind = SourceKind::parse(&row.source_kind)?;
    (row.project_id > 0).then(|| SearchCandidate {
        kind,
        project: ProjectId::new(row.project_id),
        key: row.source_key,
        title: row.title,
        body: row.body_text,
        classification: row.classification,
        truncated: row.truncated,
        updated_at: row.source_updated_at,
    })
}

/// The candidates of `tsquery` (a `forge_core::search_text` literal) after
/// skipping `offset`, at most `max_rows` ([`MAX_CANDIDATES`]), ranked by
/// cover density, then newer source, then newer row. One statement in a
/// read-only transaction with `SET LOCAL statement_timeout` =
/// [`SEARCH_STATEMENT_TIMEOUT`]. Errors are store failures (`FG-DB-…`;
/// the statement timeout is `FG-DB-0002 reason=statement_timeout`); a
/// `max_rows` or `offset` out of range is `FG-SYS-0001`.
pub async fn candidates(
    db: &Db,
    tsquery: &str,
    filter: &CandidateFilter,
    offset: u32,
    max_rows: u32,
) -> Result<Vec<SearchCandidate>, ForgeError> {
    if max_rows == 0 || max_rows > MAX_CANDIDATES || offset > MAX_OFFSET {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "search_page_out_of_range")
            .with_context("max_rows", max_rows)
            .with_context("offset", offset));
    }
    let kinds: Vec<&'static str> = filter.kinds.iter().map(|k| k.as_str()).collect();
    let rows: Vec<Row> = db
        .within("search.candidates", async {
            let mut tx = db
                .pool()
                .executor()
                .begin()
                .await
                .map_err(|e| read_query_error("search.candidates_begin", e))?;
            sqlx::query(SET_TIMEOUT_SQL)
                .execute(&mut *tx)
                .await
                .map_err(|e| read_query_error("search.candidates_timeout", e))?;
            let rows: Vec<Row> = sqlx::query_as(CANDIDATES_SQL)
                .bind(tsquery)
                .bind(&kinds)
                .bind(filter.project.map(ProjectId::get))
                .bind(filter.exclude_confidential)
                .bind(i64::from(offset))
                .bind(i64::from(max_rows))
                .fetch_all(&mut *tx)
                .await
                .map_err(|e| read_query_error("search.candidates", e))?;
            tx.commit()
                .await
                .map_err(|e| read_query_error("search.candidates_commit", e))?;
            Ok(rows)
        })
        .await?;
    Ok(rows.into_iter().filter_map(candidate).collect())
}
