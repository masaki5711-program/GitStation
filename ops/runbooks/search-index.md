# Runbook: search-index（FG-KNOW-0001、FG-KNOW-0002）

横断検索（計画 20 K2〜K4、ADR-0040）の索引 `forge.search_entries` と検索 API `GET /api/v1/search` の手順。索引は **派生のデータ**（元はプロジェクト・MR の表と、既定ブランチの先端の文書）なので、いつでも作り直せる。

文書（K4）は push のたびに購読者 `search-index` がジョブ `search.index_documents` を入れ、worker が既定ブランチの先端を読む（変わったパスだけ。最初・作り直し・型の定義の変更・読みの上限のときは全体）。機密の区分は先端での区分を行に持ち、AI の利用者には `confidential` の行を返さない。

## Trigger

- 利用者から「いま検索できません（問い合わせ番号: …）」の問い合わせ → `FG-KNOW-0001`
- ログに `forge.search.index_item`（`FG-KNOW-0002`）が出た。`reason` で分ける:
  - `truncated`: 1 件の本文が 256 KB、または語が 5 万を超え、超えた分は検索できない（行に `truncated`）
  - `subtree_too_large`: グループの移動・利用者の名前の変更で 2000 を超えるプロジェクトのパスが変わり、残りは古いパスのまま
  - `write_failed`: 作り直しで 1 つのプロジェクトの行を書けなかった（原因のコードが `cause_code`）
  - `read_bound`（文書）: 先端の文書を読みの上限（`bound` = `doc_tree` / `doc_files` / `doc_large_files` / `doc_bytes` / `doctype_files`）で一部しか読めなかった。読めた文書は索引し、読めなかった文書の行は `confidential` に倒す。状態は `failed`、`indexed_sha` は空（次の push で全体を読み直す）
  - `read_failed`（文書）: ジョブが最後の試行でも失敗した（git・ストレージ・DB。`cause_code` / `cause`）
  - `key_too_long`（文書）: パスが 1024 バイトを超え、その文書は索引しない
- `forge doctor` の `subscribers.lag` で購読者 `search-index` が遅れている・止まっている（`FG-DB-0008`、runbook subscriber-lag）
- 「作ったはずの MR・プロジェクト・文書が検索に出ない」

## Impact

- 検索だけが使えない・古い。プロジェクト・MR・git の操作には影響しない。データは失われない（索引は作り直せる）。
- 検索の失敗が 30 分を超えて続くなら SEV3。

## Diagnose（読み取りのみ）

1. ログを `request_id` で引く。`FG-KNOW-0001` の `operation`（`search_candidates` / `search_projects`）、`timeout_ms`、連鎖した `FG-DB-…`、`query_chars` / `query_terms`（問い合わせの長さと語の数。**語そのものはログに出ない**）。
2. 表の大きさと索引:
   ```sql
   SELECT source_kind, count(*), sum(truncated::int) AS truncated
     FROM forge.search_entries GROUP BY source_kind;
   SELECT pg_size_pretty(pg_total_relation_size('forge.search_entries'));
   SELECT indexrelid::regclass, pg_size_pretty(pg_relation_size(indexrelid))
     FROM pg_index WHERE indrelid = 'forge.search_entries'::regclass;
   SELECT project_id, state, last_error_code, attempts, updated_at
     FROM forge.search_index_state WHERE state <> 'ok' ORDER BY updated_at DESC LIMIT 20;
   ```
3. 購読者の位置: `SELECT name, last_seq, failures, last_error_code FROM forge.event_subscribers WHERE name = 'search-index';`
   文書のジョブ: `SELECT id, state, attempts, payload, last_error_code, updated_at FROM forge.jobs WHERE kind = 'search.index_documents' AND payload->>'project_id' = '<id>' ORDER BY id DESC LIMIT 5;`（`dead` なら `read_failed` のログと原因のコード）。先端と索引した SHA: `SELECT indexed_sha, state, last_error_code FROM forge.search_index_state WHERE project_id = <id>;` を `git -C <repo> rev-parse HEAD` と比べる（読み取り）。
4. 出ないはずの物が出る・出るはずの物が出ない: 行があるか（`SELECT source_kind, source_key, title, indexed_at FROM forge.search_entries WHERE project_id = <id>`）。行があって出ないなら、検索した人がそのプロジェクトを読めない（`ProjectRead` / MR は Reporter / 文書は `RepoBrowse`）か、AI の利用者で行が `confidential`。既定ブランチ以外の文書は索引しない（仕様）。**読めない物を出すための回避はしない。**

## Mitigate（本番は承認された手順だけ）

- **作り直し**（冪等、プロジェクトごとに 1 トランザクション、他の操作を止めない）:
  - 1 つのプロジェクト: `forge search reindex --project <id>`
  - 全体（分かち書きの規則を変えた後、復元の後、`subtree_too_large` の後）: `forge search reindex`
  - 出力の `failed` に出たプロジェクトは `FG-KNOW-0002 reason=write_failed` の原因を直してから再実行する。
  - 作り直しはプロジェクトごとに文書のジョブ（`full`）も入れる。worker が先端を読み直し、消えた文書の行も消す。
- **`read_bound`**: 1 つのリポジトリの文書が上限（既定: `.md` 2000 件、32 MB）を超えている。回避で上限を上げない。リポジトリを分けるか、上限の変更を MR で提案する。その間、読めなかった文書は AI に出ない（閉じる側）。
- **遅い検索**: DB の状態を先に見る（runbook db-unavailable）。GIN の膨張なら静かな時間に `REINDEX INDEX CONCURRENTLY forge.search_entries_tokens;`。
- **購読者が止まった**: runbook subscriber-lag。直した後は自動で追い付く。止まっていた間の変更は作り直しでも反映できる。

## Verify

- 問い合わせ番号の利用者がもう一度検索し、`200` で結果が返る。
- `SELECT count(*) FROM forge.search_index_state WHERE state = 'failed'` が 0。
- `forge doctor` の `subscribers.lag` が ok。

## 備考

- 分かち書きの規則（`forge-core::search_text`: NFKC、小文字、和文の 2-gram）を変えたら、古い行と問い合わせが合わなくなる。リリースの手順に `forge search reindex` を入れる。
- バックアップには索引を入れなくてよい（復元の後に作り直す）。
- 問い合わせの文字列はログ・監査に残さない（個人の調べ物・機密の語を含みうる）。
- `FG-KNOW-0001 reason=project_scan_limit`: 検索は読めるプロジェクトを先に決める（ADR-0040 Amendment K3-R）。2 万プロジェクトを超えたら検索を断る。回避はせず、SQL の上位集合への移行（ADR の改訂）を起票する。
- 文書の行の `source_updated_at` は、その変更が索引に入った時刻（push の時刻にほぼ等しい）。
- `subtree_too_large` の残りのプロジェクトは `search_index_state` に `failed`（`FG-KNOW-0002`）で残る。`forge search reindex` で `ok` に戻る。
