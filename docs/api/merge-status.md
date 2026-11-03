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
  `ff_not_possible` を出す（ジョブはロックの下でもう一度決める）。`nothing_to_merge` はジョブだけが出す。

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
| `checks[]` | 配列 | target のマージの条件が求める必須のチェック: `{ "name", "status", "conclusion" }`。**`status: null` は報告が無い**（成功ではない） |
| `discussions` | オブジェクト / null | マージの条件がスレッドの解決を求めるとき `{ "unresolved": 件数 }`、求めないとき null |
| `codeowners` | オブジェクト | `{ "enforced": 真偽, "suspended_until": 文字列 / null }`（一時停止中だけ期限。期限を過ぎれば `enforced: true`） |

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
| `config_change_needs_maintainer_approval` | null | 設定のファイルの変更に Maintainer の承認が無い |
| `changed_paths_unknown` | `reason=<r>` | 変更されたパスを読み切れない（閉じる側） |

`work_item_link_missing` は作業項目（I1）が入るまで出ない（設定できない）。

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
  "codeowners": {"enforced": true, "suspended_until": null}
}
```

理由ごとの対処は runbook `merge-blocked`（承認・マージの条件）と `codeowners`。
