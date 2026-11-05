# マージの状態の契約（`gstn mr status --json`、schema_version 1）

M3 R6（ADR-0024 §7）。エージェント・画面・kanflow が「この MR は今マージできるか、できないならなぜか」を **1 つの答え** で読むための
安定した JSON。`GET /api/v1/projects/{id}/merge-requests/{iid}/status`（`MergeRequestRead`、トークンは `read_api`）の本文と、
`gstn mr status --project <id> --iid <n> --json` の出力は同じ。

## 約束

- `schema_version` が同じ間は、欄を **足すことはあっても、名前を変えたり消したり意味を変えたりしない**。変えるときは `schema_version` を
  上げる。読む側は知らない欄を無視する。
- `reasons` は `evaluate_merge()` の止める理由の **すべて**（最初の 1 つで止めない）を、ADR-0024 §7 の **固定の順** で並べる。
  最初の要素が画面の「止まっている理由の 1 行」（計画 08）。
- 読み取りだけ（何も書かない、確認を積まない）。マージはロックの下でもう一度判定するので、`mergeable: true` でもマージの要求が
  断られることはある（その間に head・target・条件が変わったとき）。逆に `merge_request.approved` イベントの `counted` はマージの可否の
  判断に使わない（ADR-0024 §6）。この契約を使う。
- 評価する方式はプロジェクトの既定の方式（`method`）。`fast_forward` なら target の先端が head の祖先かも確かめて
  `ff_not_possible` を出す（ジョブはロックの下でもう一度決める）。その確認で git が失敗したときは早送りを「不明」として
  `ff_not_possible` を出さず（状態の取得は失敗にしない）、warn `forge.review.status_ff_unknown`（`error_code`・`request_id`）を 1 行出す。
  `nothing_to_merge` はジョブだけが出す。

## 欄

| 欄 | 型 | 意味 |
|---|---|---|
| `schema_version` | 整数 | 1 |
| `iid` | 整数 | プロジェクトの中の MR の番号 |
| `head_sha` | 文字列 / null | 今の head。版がまだ無いと null（`reasons` に `mergeability_unknown`） |
| `target_branch` | 文字列 | `refs/heads/` を除いた target |
| `method` | 文字列 | 評価した方式（`merge_commit` / `squash` / `fast_forward`） |
| `mergeable` | 真偽 | 止める理由が無い |
| `reasons[]` | 配列 | `{ "reason": 名前, "detail": 文字列 / null }`。下の表 |
| `approvals` | オブジェクト / null | 承認の側（版が無いと null）: `rules[]`（`name`、`applies`、`required`、`counted`。保護ブランチの最低の承認数は `name = "@protected_branch"`）、`approvals[]`（`approver_id`、`username`、`counted`、`uncounted_reason`）、`codeowners[]`（`pattern`、`owners`、`satisfied`。CODEOWNERS の強制が一時停止中は空） |
| `checks[]` | 配列 | target のマージの条件が求める必須のチェック: `{ "name", "status", "conclusion" }`。**`status: null` は報告が無い**（成功ではない）。値は Checks API（`docs/api/checks.md`、M3 C1）で **今の head** に報告された結果（古い head への報告は数えない） |
| `discussions` | オブジェクト / null | マージの条件がスレッドの解決を求めるとき `{ "unresolved": 件数 }`、求めないとき null |
| `work_item_link` | オブジェクト / null | M3 I1d で追加。マージの条件が作業項目のリンクを求めるとき `{ "linked": 真偽 }`（プロジェクトの **今の** 連携の結びつけの作業項目への開いたリンクがあるか）、求めないとき null |
| `codeowners` | オブジェクト | `{ "enforced": 真偽, "suspended_until": 文字列 / null }`（一時停止中だけ期限。期限を過ぎれば `enforced: true`） |
| `viewer` | オブジェクト | 呼び出した人が **今** できること（U2 で追加。下の表） |

## `viewer`（呼び出した人ができること）

画面は、押しても断られるボタンを出さないためにこれを読む。各欄は、その操作の API が判定に使うのと **同じ入力**（読み込んだ MR、
target の保護、頼む変更）で `authorize()` を試しに呼んだ答え。判定の規則をここで持たない。**ヒントであって許可ではない**:
実際の操作はその API がもう一度判定し、コマンドがロックの下でさらにもう一度判定する（その間に役割や保護が変わればボタンを
押しても断られる）。トークンで読んだときは、そのトークンのスコープで判定する（`read_api` だけのトークンはどれも `false`）。

| 欄 | 型 | 意味 |
|---|---|---|
| `can_approve` | 真偽 | `POST …/approve` が許される（Developer 以上・人だけ・**作成者は常に `false`**・トークンは `merge_approve`） |
| `can_unapprove` | 真偽 | `POST …/unapprove` が許される（承認と同じ判定） |
| `can_merge` | 真偽 | `POST …/merge` が許される（target の保護の最低のプッシュ役割。保護が無ければ Developer・トークンは `merge_execute`）。**今マージできるかは `mergeable`** |
| `can_mark_ready` | 真偽 | `PATCH …/{iid}` で `draft: false` が許される（作成者、または他人のドラフトなら Maintainer 以上） |
| `approved_by_me` | 真偽 | 呼び出した人の生きている承認がある（版が無ければ `false`） |

- `can_unapprove` は「取り消す権限がある」だけで、取り消す承認があるとは限らない。画面は **`approved_by_me` と組で** 使う
  （`can_unapprove && approved_by_me` のときだけ「承認を取り消す」を出す）。同じく `can_approve && !approved_by_me` で「承認する」。
- `can_mark_ready` は **ドラフトのときだけ** 意味がある（ドラフトでない MR でも `true` になりうる。ボタンはドラフトの間だけ出す）。
- 欄の裏の読み取り（target の保護）が失敗・時間切れのときは、状態の答えは変えずに **欄をすべて `false`** にし、warn
  `forge.web.viewer_flags_closed`（`error_code`、`request_id`、`read`）を 1 行出す（ヒントは閉じる側に倒す）。

## `reasons[].reason` の名前（この順に並ぶ）

| 名前 | `detail` | 意味 |
|---|---|---|
| `not_open` | null | MR が開いていない（merged / closed） |
| `draft` | null | ドラフト |
| `merge_in_progress` | null | 実行中のマージがある |
| `source_branch_missing` | null | ソースブランチが無い |
| `mergeability_unknown` | null | 今の (head, target の先端) の組でマージできるかが決まっていない（確認の待ち・失敗、版が無い） |
| `conflict` | null | 衝突する |
| `ff_not_possible` | null | 早送りできない（方式が `fast_forward` のとき） |
| `nothing_to_merge` | null | target が既に head（早送り。ジョブだけが出す） |
| `approvals_missing` | `rule=<名前> required=<n> counted=<m>` | 承認のルール、または `@protected_branch`（保護ブランチの最低の数）の承認が足りない |
| `codeowner_approval_missing` | `pattern=<p> owners=<@a,@b>` | 変更されたパスの CODEOWNERS の所有者の承認が無い |
| `codeowners_invalid` | `line=<n> reason=<r>` | target の CODEOWNERS を使えない（runbook codeowners） |
| `required_check_missing` | `name=<check>` | 必須のチェックの報告が今の head に無い |
| `required_check_not_successful` | `name=<check> status=<s> conclusion=<c\|none>` | 必須のチェックが `completed` / `success` でない（`neutral` を含む） |
| `unresolved_discussions` | `count=<n>` | 未解決のスレッドがある（マージの条件が求めるときだけ） |
| `work_item_link_missing` | null | マージの条件が作業項目のリンクを求め、MR にプロジェクトの今の結びつけの作業項目（`PBI-<n>`）への開いたリンクが無い（M3 I1d。結びつけの無いプロジェクトは満たせない） |
| `config_change_needs_maintainer_approval` | null | 設定のファイルの変更に Maintainer の承認が無い |
| `changed_paths_unknown` | `reason=<r>` | 変更されたパスを読み切れない（閉じる側） |

`work_item_link_missing`（M3 I1d）: タイトルかソースブランチの名前に `PBI-<n>` を入れる（検出は `work-item-sync`）か、
`gstn mr link-work-item --ref PBI-<n>`（`PUT …/merge-requests/{iid}/work-items/{ref}`、Developer 以上）でリンクすると外れる。
結びつけを解除するとリンクは終わる（I1b）ので、解除・付け替えの後はまたこの理由で止まる。runbook `merge-blocked`。

OpenAPI（`docs/api/openapi.json`）では `reason` は上の名前の enum（`StatusReasonName`）。名前は足すことはあっても変えない
（`status_reason_schema_lists_every_block_reason` が `BlockReason` の全種類・この表・enum の一致を確かめる）。読む側は知らない名前を
「止まっている」として扱う。

## 読む側の注意（`detail` の読み方）

- `detail` は `key=value` を空白で区切った文字列。値は次の ` key=`（空白、英小文字か `_` の並び、`=`）の手前までと読む。こう読むと、
  空白を含む値（ルールの名前など）もそのまま取れる。
- **限界**: 値そのものに ` key=` の形（空白のあとに英小文字か `_` が続き `=` が来る）が入ると、そこで値が切れ、残りが別の組として
  読まれる。今のサーバーの名前（承認のルール、チェックの名前、CODEOWNERS のパターン）はこの形を取らないが、取る名前ができたら
  画面の 1 行の値が違って見えるか、汎用の 1 行（「今はマージできません」）になる。例外にはしない（Web の `blockedReason()`、
  `web/src/features/mr/blockedReason.ts`）。
- 知らない `reason` の名前、読めない `detail`（数でない件数、欠けた鍵）は、汎用の 1 行にする。`schema_version` が同じ間に名前が
  足されることがあるので、読む側はそれで止まらないこと。

## 例

```json
{
  "schema_version": 1,
  "iid": 3,
  "head_sha": "2222222222222222222222222222222222222222",
  "target_branch": "main",
  "method": "merge_commit",
  "mergeable": false,
  "reasons": [
    {"reason": "approvals_missing", "detail": "rule=@protected_branch required=1 counted=0"},
    {"reason": "required_check_missing", "detail": "name=ci/gates"}
  ],
  "approvals": {
    "rules": [{"name": "@protected_branch", "applies": true, "required": 1, "counted": 0}],
    "approvals": [],
    "codeowners": []
  },
  "checks": [{"name": "ci/gates", "status": null, "conclusion": null}],
  "discussions": {"unresolved": 0},
  "work_item_link": null,
  "codeowners": {"enforced": true, "suspended_until": null},
  "viewer": {
    "can_approve": true,
    "can_unapprove": true,
    "can_merge": true,
    "can_mark_ready": true,
    "approved_by_me": false
  }
}
```

理由ごとの対処は runbook `merge-blocked`（承認・マージの条件）と `codeowners`、チェックの報告は `checks-reporting`。

## 必須のチェックの結果（M3 C1）

この契約の欄は変えずに、`checks[]` と `required_check_*` の中身が Checks API（`docs/api/checks.md`）の報告から来るようになった。

- `checks[]` の各要素は、マージの条件が求める名前ごとに、**MR の今の head** に報告された実行（`status`、`conclusion`）。報告が無ければ
  `status: null`（`required_check_missing`）。`completed` / `success` 以外（`neutral`・`queued`・`in_progress`・`failure` など）は
  `required_check_not_successful`。
- 状態の取得は保存された実行をその場で読む。**読めなかったとき（DB の失敗・時間切れ・読めない行の `FG-DB-0022`）は状態の取得そのものが
  失敗する**（`required_check_missing` として見せない）。マージの条件が必須のチェックを求めないときは実行を読まない。
- マージの要求とマージのジョブは、MR のロックの下のトランザクションで同じ SQL で実行を読み直す（ジョブは要求の時点ではなく、ジョブの
  ゲートの時点の結果で決める。その間に `failure` になれば `superseded`）。

## マージの設定（`GET` / `PUT /api/v1/projects/{id}/merge-settings`）の `viewer` と同時編集

この契約の外（`schema_version` を持たない）だが、同じ U2 で次を足した。

- 答え（`GET`、`PUT …/merge-settings`、`PUT …/codeowners-enforcement`）に `viewer`（PUT の答えの `viewer` はロックの前に読んだ
  事実での答えで、ヒント。読み取りが失敗したときはすべて `false` で warn を 1 行）:
  `{ "can_manage", "can_loosen", "can_suspend_codeowners" }`。上と同じく `authorize()` の試しの答え。
  `can_manage` は承認を緩めない変更、`can_loosen` はコミッターの承認を数える・プッシュで承認を持ち越す変更（プロジェクトの
  **すべての保護以上の役割**が要る）、`can_suspend_codeowners` は CODEOWNERS の一時停止（同じく保護以上の役割）。
  Developer には読み取りだけの画面を出す（止まっている理由は見せる）。
- `updated_at` はマイクロ秒まで（RFC 3339、UTC）だが、クライアントにとっては **不透明な文字列**: 読んだ文字列を **そのまま**
  返す（`Date` に変換して戻さない。ミリ秒に丸まって必ず 409 になる）。`PUT …/merge-settings` は **`expected_updated_at` が必須**: 読んだときの
  `updated_at`（既定のまま一度も保存されていなければ `null`）を送る。ロックの下の値と違えば `409 FG-REVIEW-0017`（何も書かない）。
  CODEOWNERS の切り替えや期限切れの自動復帰も `updated_at` を進めるので、その後の保存は読み直しが要る。欄が無い要求は
  `FG-WEB-0006`（古いクライアントが他人の変更を黙って上書きしないため）。`gstn merge-settings set` は読んだ値を自分で送る。

## 承認の規則と保護したブランチのマージの規則の同時編集と承認者の名前（U2 の続き）

この契約の外（`schema_version` を持たない）。マージの設定と同じ考え方を、規則の書き込みにも広げた。

- `GET /api/v1/projects/{id}/approval-rules` の各規則と `PUT …/approval-rules/{name}` の答えに `updated_at`（マイクロ秒までの
  不透明な文字列）を足した。`PUT …/approval-rules/{name}` は **`expected_updated_at` が必須**: 読んだ規則の `updated_at` を
  そのまま送る。**`null` は「この名前の規則はまだ無い」（作るだけ）** の意味で、同じ名前の規則が既にあれば置き換えずに
  `409 FG-REVIEW-0018`（`reason=rule_exists`）。値が違えば `stale_rule`、読んだ後に消されていれば `rule_gone`（黙って作り直さない）。
  どれも何も書かない。欄が無い要求は `FG-WEB-0006`。
- M3 I1d: `PUT …/merge-rule` は **`require_work_item_link` も必須**（`expected_updated_at` と同じく、欄が無ければ `FG-WEB-0006`
  `reason=missing_field`。その欄を知らない古いクライアントが条件を黙って外さないため）。
- `GET /api/v1/projects/{id}/protected-refs` の `merge_rule` と `PUT …/protected-refs/{rule_id}/merge-rule` の答えの
  `merge_rule` に `updated_at` を足した。`PUT …/merge-rule` も **`expected_updated_at` が必須**（`null` はマージの規則がまだ
  無いとき）。違えば `409 FG-REVIEW-0019`（`reason` は上と同じ 3 つ）。
- 承認の規則の答えは承認者を名前つきで返す: `users: [{ "id", "username", "display_name" }]`、
  `groups: [{ "id", "full_path", "display_name" }]`（`user_ids` / `group_ids` と同じ順。`user_ids` / `group_ids` も残す）。
  見える範囲は規則そのものと同じ（`ProjectRead`）。新しい経路は無い。名前を引けない承認者（読み取りの失敗も）は `users` /
  `groups` から外れ、id は `user_ids` / `group_ids` に残る（warn `forge.web.approver_names_incomplete`）。数が合わない規則は、
  そのまま保存し直すと外れた承認者が消えるので、画面は保存を出さない。書き込みは利用者を `username`、グループを `full_path` で
  名指しするので、画面は答えの名前のまま保存し直せる。
- `gstn approval-rule set` と `gstn merge-rule set` は先に読み、読んだ `updated_at`（無ければ `null`）を自分で送る。409 なら
  もう一度実行する。
