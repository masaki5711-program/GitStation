# イベントの履歴 API の契約（連携の feed、schema_version 1）

M3 I1c（計画 02 §5.7・§8.2・§11.2、ADR-0016 §7 と Amendment I1c、計画 11 §5 I1）。kanflow（K3 の `GitStationSource`）は、
連携に結びついたプロジェクトで起きたことを **定期取得でも取りこぼさず** にこの API で読む。答えの例（kanflow の偽の GitStation が返す
golden）は `docs/api/integration-feed.example.json`。forge-web のテスト `the_feed_answers_the_golden_example_shape` が、実際の答えの欄と
例の欄が同じであることを確かめる。

## 約束

- `schema_version` が同じ間は、欄を **足すことはあっても、名前を変えたり消したり意味を変えたりしない**。読む側は知らない欄・知らない
  イベントの種類（`type`）を読み飛ばす。欄を足すときは、同じ変更で例の JSON も更新する。
- **取りこぼさない**: イベントは状態の変更と同じトランザクションで Outbox に書かれ、欠番のない `seq` の順に返る。カーソルは不透明な
  文字列（AES-256-GCM で封じた位置。**読まない・作らない**）。
- **少なくとも 1 回**: kanflow は処理が終わってからカーソルを保存する。途中で落ちたら保存したカーソルから読み直す。重複は `id`（`evt_…`）で除く。
- **同じカーソルからは同じイベント**が返る（その間に見える範囲が変われば 410）。
- **作業項目より先には返さない**: 読む上限は `min(high_seq, work-item-sync の last_seq)`。イベントは、その `work_items` が決まってから返る。
- **黙って途中から返さない**: 続けられないカーソルは 410（下の表）。
- **同時に 1 つ**: 同じ連携の読み取り（`…/events`・`…/events/cursor`）が処理中なら、2 つ目はすぐに 429 `FG-WEB-0010`（`Retry-After: 1`）。並べて待たせない。kanflow は 1 つずつ読む。
  時間あたりの回数の制限はまだない。1 回は最大 500 件・約 2 MiB・30 秒。

## 誰が読めるか（`IntegrationEventRead` / `IntegrationEventCursorRead`、トークンは `read_api`）

- **その連携のサービスアカウント**のトークンだけ（`read_api`。`api` も満たす）。
- 別の連携の `{id}`・存在しない `{id}`・人（**管理者のセッションと `admin` のトークンも**）・ボット: すべて同じ `FG-REPO-0005`（404）。
- エージェントのトークン（`reason=agent_token`）・`Forge-Act-As` の付いた要求（`reason=act_as_refused`、代理での読み取りは常に不可）・
  無効にした連携（`reason=integration_disabled`）: `FG-AUTH-0003`（403）。無効のサービスアカウント: `FG-AUTH-0004`。
- 見えるイベント: 結びつけたプロジェクトのうち、サービスアカウントが **Reporter 以上のメンバー**で、削除待ちでないもの、の
  **連携の分類**（ADR-0016 §5: `merge_request.*`、`repository.pushed`、`project.created` など、`check.completed`、`work_item.*`）の
  イベントを、連携の `event_types` で狭めたもの。判定は読むたびに `authorize()` で行う（`prove_visible`）。
- **決して出ないもの**: `project_id` のないイベント（インスタンス全体）、`member.*`・`protected_ref.*`・`project.visibility_changed`
  （maintainer の分類）、`token.*` など（admin の分類）。結びつけを解除したプロジェクトは、**解除の前のイベントも後のイベントも**出ない。

## `GET /api/v1/integrations/{id}/events?cursor=…&limit=…`

| パラメーター | 内容 |
|---|---|
| `cursor` | 前の答えの `next_cursor`、イベントの `cursor`、または `…/events/cursor` の答え。無ければ保持されている最も古いイベント（`retained_from`）から。保持期間の中なら **結びつけより前の履歴も**返る（見えるかは読む時点で判定する）。最初の同期は `…/events/cursor` → 開発状況 API（I1d、`docs/api/dev-status.md`）→ その位置から、の順にする |
| `limit` | 1〜500（既定 100）。答えは約 2 MiB でも切れる（`has_more: true`） |

それ以外のパラメーター・同じパラメーターの繰り返し・`limit` の範囲外・空のカーソルは `FG-WEB-0006`（400、`reason=param_invalid`）。

答え（200、`Cache-Control: private, no-store`）:

| 欄 | 内容 |
|---|---|
| `schema_version` | `1` |
| `events[]` | `seq` の順 |
| `events[].id` | `evt_` + 32 桁の 16 進数。重複の除去に使う |
| `events[].cursor` | このイベントの直後の位置（このイベントまで処理したら、ここから続けられる） |
| `events[].type` | `<集約>.<過去形の動詞>`。知らない種類は読み飛ばす |
| `events[].schema_version` | `data` の版 |
| `events[].occurred_at` | UTC、RFC 3339、ミリ秒まで |
| `events[].project_id` | forge のプロジェクトの ID |
| `events[].actor` | `kind`（`user` / `service_account` / `user_via_service_account` / `system`）、`user`（`id`・`username`・`display_name`。`system` なら `null`）、`via`（代理実行したサービスアカウント、無ければ `null`） |
| `events[].actor.user.email` | トークンが **`user_email`** を持つときだけ。持たないとき・値の無い利用者（サービスアカウントなど）は欄ごと無い |
| `events[].actor.user.employee_number` | トークンが **`user_employee_number`** を持つときだけ（計画 11 Q7）。持たないとき・値の無い利用者は欄ごと無い |
| `events[].work_items[]` | `{provider: "kanflow", project_id: <kanflow のプロジェクト ID>, ref: "PBI-<n>"}`。無ければ空の配列。**ブランチの削除・マージ・クローズの後も残る** |
| `events[].data` | そのイベントの種類と版の `data`（その時点の表示名・タイトル・ブランチ名のスナップショット。本文や差分は入らない） |
| `next_cursor` | 次に読む位置。見えるイベントが残っていなければ **先頭まで進む**（見えないイベントの数は分からない） |
| `has_more` | `next_cursor` の後に見えるイベントが残っているときだけ `true` |

`username`・`display_name` は読み取りの時点のもの。

## `GET /api/v1/integrations/{id}/events/cursor`

`{"schema_version": 1, "cursor": "…"}`: その連携に見える範囲の **今の先頭**の位置。パラメーターは無い（あれば `FG-WEB-0006`）。

## 410 と 400

| 答え | 本文の `reason` | 意味 |
|---|---|---|
| 410 `FG-INT-0007` | `retention` | カーソルの後のイベントが保持（既定 90 日、上限 1.5 倍）で消えた、またはカーソルが保持期間の 1.5 倍より古い |
| 410 `FG-INT-0007` | `epoch_mismatch` | DB の復元・作り直しの後に `epoch` を作り直した（`forge admin events new-epoch`） |
| 410 `FG-INT-0007` | `ahead_of_head` | カーソルが今の先頭より先を指す（DB を前の時点に戻した） |
| 410 `FG-INT-0007` | `visible_set_changed` | 見える範囲が変わった: プロジェクトの結びつけ・解除、**同じプロジェクトの解除と結び直し**（結びつけの世代が変わる）、サービスアカウントのロール、プロジェクトの削除待ち、`event_types` |
| 400 `FG-INT-0008` | （無い） | この連携のカーソルではない（改変・別の連携・別の一覧・書式の誤り・鍵の喪失）。値はログにも出ない |

410 の本文は `error_code`・`user_message`・`request_id` と `reason` だけ。

**410 からの復旧**（この順番でないと、取り直しの間の変化を取りこぼす）:

1. `GET /api/v1/integrations/{id}/events/cursor` で今の位置を取って保存する
2. 開発状況 API（`GET …/dev-status?state=all` を `next_cursor` が `null` になるまで。`docs/api/dev-status.md`）で全体を取り直す
3. 1 の位置から `GET …/events` を読む

## 運用

- 読み取りは連携ごとに 1 分に 1 回まで `forge.integration_feed_reads` に記録する。24 時間読みに来ない・未読の最も古いイベントが保持期間の
  半分より古い連携は `forge doctor` の `integrations.feed_lag`（`FG-INT-0009`、warn）。Runbook `integration-feed-lag.md`。
- `feed.require_sink_compare` は M3 では `false` だけ（監査の封印の送出先がまだないため。doctor `feed.sink_compare` が警告する）。
