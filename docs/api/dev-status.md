# 開発状況 API の契約（連携の dev status、schema_version 1）

M3 I1d（計画 02 §5.2・§8.2・§11.2、ADR-0016 Amendment I1d、計画 11 §5 I1）。kanflow（K3 の `GitStationSource`）は、PBI の画面に
「この PBI にリンクした MR が今どうなっているか」をこの API で出し、イベントの履歴（`docs/api/integration-feed.md`）が 410 を返したときに
全体をこの API で取り直す。答えの例（kanflow の偽の GitStation が返す golden）は `docs/api/dev-status.example.json`。forge-web のテスト
`the_dev_status_answers_the_golden_example_shape` が、実際の答えの欄と例の欄が同じであることを確かめる。

## 約束

- `schema_version` が同じ間は、欄を **足すことはあっても、名前を変えたり消したり意味を変えたりしない**。読む側は知らない欄を読み飛ばす。
  欄を足すときは、同じ変更で例の JSON も更新する。
- **開いている・マージ済み・クローズの MR をすべて**返す（`state=all` が既定。02 §8.2: マージせずにクローズした MR も「クローズ」で出る）。
  出るのは **今開いているリンク**（タイトル・ソースブランチからの自動のリンクと、人のリンク）の MR。リンクはブランチの削除・マージ・クローズの
  後も残る。リンクを外した MR・結びつけを解除したプロジェクトの MR は出ない。
- **`as_of_seq` とその `feed_cursor`**: 答えは 1 つのスナップショットで読み、`seq <= as_of_seq` のイベントの変化はすべて反映している
  （`as_of_seq` = `min(high_seq, work-item-sync の last_seq)`、履歴の読む上限と同じ）。`feed_cursor` はその位置の履歴のカーソルで、
  **そこから `GET …/events` を読めば取りこぼさない**（その後のイベントの変化が既に反映されていることはある: 少なくとも 1 回、`id` で重複を除く）。
  `feed_cursor` は `GET …/events/cursor` がその時点で返すものと同じ位置。
- **チェックが無いことは成功ではない**: `checks` が空なら `checks_state: "none"`。成功は `"success"`（すべての実行が `completed` / `success`）。
- 同じ連携の読み取り（履歴・カーソル・開発状況）は同時に 1 つ。2 つ目はすぐに 429 `FG-WEB-0010`（`Retry-After: 1`）。1 回は最大 30 秒。

## 誰が読めるか（`IntegrationDevStatusRead`、トークンは `read_api`）

イベントの履歴と **同じ**（`docs/api/integration-feed.md` の「誰が読めるか」）: その連携のサービスアカウントのトークンだけ。別の連携・
存在しない連携・人（管理者のセッションと `admin` のトークンも）・ボットは `FG-REPO-0005`（404）、エージェントのトークン・`Forge-Act-As`・
無効にした連携は `FG-AUTH-0003`（403）。見える範囲も履歴と同じ判定（`prove_visible`）: 結びつけたプロジェクトのうち、サービスアカウントが
Reporter 以上のメンバーで削除待ちでないもの。連携の `event_types` が `merge_request.*` を 1 つも許さないなら MR は出ない（作業項目だけ出る）。
gstn: `gstn integration dev-status <id> --ref PBI-<n>` と `gstn integration dev-status-list <id>`（サービスアカウントのトークンで）。

## `GET /api/v1/integrations/{id}/work-items/{ref}/dev-status?project_id=…&state=…`

| パラメーター | 内容 |
|---|---|
| `{ref}` | `PBI-<n>` の正規形（先頭の 0 なし）。それ以外は `FG-WEB-0006`（`param=ref rule=format`） |
| `project_id` | kanflow のプロジェクト ID（`A-Z a-z 0-9 . _ : -` の 1〜100 文字）。無ければ、結びつけたすべての kanflow のプロジェクトの、その参照の作業項目 |
| `state` | `all`（既定）・`open`・`merged`・`closed` |

答え（200、`Cache-Control: private, no-store`）: `{schema_version: 1, as_of_seq, feed_cursor, work_items[]}`。`work_items` は
結びつけた（見える）kanflow のプロジェクトごとに 1 つ（`project_id` を付ければ 1 つ）。まだ誰も名指していない参照は `merge_requests: []`。
**その kanflow のプロジェクトが見える結びつけに無い**（`project_id` が結ばれていない、または連携に見える結びつけが無い）ときは
`404 FG-REPO-0005`（ログは `resource=work_item reason=unbound_ref`）。

## `GET /api/v1/integrations/{id}/dev-status?cursor=…&limit=…&state=…`

すべての作業項目（見える結びつけの kanflow のプロジェクトのもの。リンクが今は無いものも `merge_requests: []` で出る）を、作業項目の id の順に
ページで返す。

| パラメーター | 内容 |
|---|---|
| `cursor` | 前のページの `next_cursor`（不透明。この連携のもの）。無ければ最初のページ（今の時点） |
| `limit` | 1〜100（既定 50）作業項目。1 ページの MR が約 1000 件を超えるとそこで切る（`has_more: true`） |
| `state` | `all`（既定）・`open`・`merged`・`closed` |

答え: `{schema_version: 1, as_of_seq, feed_cursor, work_items[], next_cursor, has_more}`。**`as_of_seq` と `feed_cursor` はどのページも最初の
ページの位置**（ページの間の変化はその後のイベントとして履歴に出る）。`next_cursor` は残りがあるときだけ文字列、最後のページで `null`。
ページのカーソルは履歴のカーソルと同じ規則で確かめる: この連携のものでない・改変・書式の誤りは `400 FG-INT-0008`、続けられないときは
`410 FG-INT-0007`（`reason` = `epoch_mismatch` / `ahead_of_head` / `retention` / `visible_set_changed`。たどる途中で結びつけ・ロール・
`event_types` が変わったら `visible_set_changed`）。410 なら最初のページからやり直す。

## 欄（`work_items[]`）

| 欄 | 内容 |
|---|---|
| `provider` | `kanflow` |
| `project_id` | 結びつけの kanflow のプロジェクト ID |
| `ref` | `PBI-<n>` |
| `merge_requests[]` | 開いたリンクの MR。新しい順、最大 100 |
| `merge_requests[].project` | `{id, path}`。`path` は今のパス（表示用。読めなければ `null`） |
| `merge_requests[].iid`・`title`・`draft` | MR の番号・今のタイトル・ドラフト |
| `merge_requests[].state` | `open` / `merged` / `closed` |
| `merge_requests[].source_branch`・`target_branch` | `refs/heads/` を除いた名前 |
| `merge_requests[].head_sha` | 今の head（版が無ければ `null`） |
| `merge_requests[].created_at`・`updated_at`・`merged_at`・`closed_at` | UTC、RFC 3339。`merged_at` はマージが適用された時刻（マージ済みでなければ `null`）、`closed_at` はマージせずに閉じた時刻 |
| `merge_requests[].approvals.given` | 今の head と target への生きている承認の数（数えられるかは問わない。マージできるかは `GET …/merge-requests/{iid}/status`） |
| `merge_requests[].checks[]` | 今の head に報告されたチェックの実行（M3 C1）: `{name, status, conclusion}`。古い head への報告は出ない |
| `merge_requests[].checks_state` | `none`（報告が無い）・`pending`（完了していない実行がある）・`failure`（`failure` / `error` / `action_required`）・`neutral`・`success` |
| `merge_requests[].link` | `{source: auto / manual, linked_at}` |
| `merge_requests_truncated` | 100 件を超えるリンクがある |

## 410 からの復旧と最初の同期

1. `GET /api/v1/integrations/{id}/events/cursor` で今の位置を取って保存する
2. `GET …/dev-status`（`state=all`）を `next_cursor` が `null` になるまで読み、kanflow の開発状況を置き換える
3. 1 の位置から `GET …/events` を読む（2 の最初のページの `feed_cursor` からでもよい。どちらも取りこぼさない）

2 の途中で 410 になったら 1 からやり直す。
