# Checks API の契約（`gstn checks`、schema_version 1）

M3 C1（ADR-0024 §10・§11 と末尾の「実装で決めたこと（C1）」、計画 02 §5.4）。CI や kanflow のサービスアカウントが **コミットに** チェックの
結果を報告し、保護ブランチのマージの条件（必須のチェック）がそれを読む。`gstn mr status --json`（`docs/api/merge-status.md`）の
`checks[]` は、ここで報告された **MR の今の head** の結果を出す。

## 約束

- `schema_version` が同じ間は、欄を **足すことはあっても、名前を変えたり消したり意味を変えたりしない**。読む側は知らない欄を無視する。
- チェックは **コミットの SHA に結ぶ**。同じプロジェクト・同じコミット・同じ名前の報告は **1 行** で、もう一度報告すればその行を上書きする
  （PATCH は無い。`id` は変わらず、`revision` が 1 つ増える）。
- 必須のチェックは「MR の今の head に、その名前の `completed` かつ `success` がある」ときだけ満たされる。**報告が無いことは成功ではない**
  （`required_check_missing`）。`neutral`・`queued`・`in_progress`・`failure` などは `required_check_not_successful`。古い head への報告は
  新しい head に数えない（push の後はもう一度報告する）。target が進んだときにチェックを「古い」とする扱いは M4（計画 04 M-2）。
- `check.completed` のイベント（Webhook・イベントの履歴）は **結論が変わった報告のときだけ** 出る（同じ結論の報告を繰り返しても増えない）。
  重複の鍵は `check_completed:<check_run_id>:<revision>`。
- `details_url` は人のためのリンクで、**サーバーは開かない**（http / https だけ。資格情報を含む URL は拒む）。答えと一覧に
  そのまま出るので、**秘密（トークン、署名つき URL の鍵）を入れない**。

## 誰が書けるか（`CheckWrite`、トークンは `api`）

| 名前空間 | 書ける主体 |
|---|---|
| `ci/`・`kanflow/` | その名前空間を **そのプロジェクトについて** 管理者から割り当てられたサービスアカウントで、そのプロジェクトの **Developer 以上のメンバー**（割り当てはプロジェクトごと。別のプロジェクトの割り当て・`kanflow` だけの割り当ては `ci/` を書けない）。人（管理者を含む）・ボット・エージェントのトークンは拒否（`FG-AUTH-0003`、`reason=not_service_account` / `agent_token`）。割り当てが無い: `reason=namespace_not_granted` |
| `forge/` | forge 自身の worker だけ（HTTP からは **管理者も** 拒否。`reason=forge_namespace_internal`）。X2 から `forge/spec-drift`、X7 から `forge/risk`、X4 から `forge/spec-lint` と `forge/spec-trace`、計画 13 段 2 から `forge/doc-lint` を書く（下の「forge 自身のチェック」） |
| `ext/` | C1 では誰も書けない（`reason=ext_namespace_unavailable`）。`ext/` は必須のチェックにもできない（FG-REVIEW-0016） |

- 見えないプロジェクト（サービスアカウントはメンバーのプロジェクトだけが見える）は、存在しないプロジェクトと同じ `FG-REPO-0005`（404）。
- 削除待ちのプロジェクトは `FG-REPO-0006`。`read_api` だけのトークンは `FG-AUTH-0002`。
- 読むのは `CheckRead`（`read_api`）: リポジトリの閲覧 API（`RepoBrowse`）と同じ人が読める。

## `POST /api/v1/projects/{id}/commits/{sha}/checks`（`gstn checks report`）

`{sha}` は **小文字の完全なコミット ID**（40 / 64 桁）。ブランチかタグから **届かない** コミット・存在しないコミットは、どちらも
`FG-REPO-0023`（`reason=ref_not_found`、404）で、何も書かない。届くかを時間内に決められなかったときも同じ答えで、何も書かない。
形の正しくない `{sha}` は `FG-REPO-0005`（404）。

本文（JSON、全体で 64 KiB まで。超えると読む前に `FG-WEB-0009`）:

```json
{
  "name": "ci/gates",
  "status": "completed",
  "conclusion": "success",
  "summary": "all green",
  "details_url": "https://ci.example.test/runs/1",
  "annotations": [
    {"path": "src/lib.rs", "start_line": 3, "end_line": 4, "level": "warning", "message": "unused variable", "title": "lint"}
  ]
}
```

| 欄 | 規則（破ると `FG-REVIEW-0020`（422）と `reason`） |
|---|---|
| `name` | `<名前空間>/<名前>`。名前空間は `ci` / `forge` / `kanflow` / `ext`、名前は英小文字・数字・`.`・`_`・`-`・`/` の 1〜100 バイトで、英小文字か数字で始まり、`/` で終わらず `//` を含まない（`name_invalid`。名前空間は判定の入力なので、この検査だけは権限の判定の前） |
| `status` | `queued` / `in_progress` / `completed`（`status_invalid`） |
| `conclusion` | `completed` のときだけ、かつ必ず: `success` / `neutral` / `failure` / `error` / `action_required`（`conclusion_invalid`、`completed_without_conclusion`、`conclusion_without_completed`） |
| `summary` | 省略は空。**プレーンテキスト**（サーバーも画面も HTML / Markdown として描画しない）。64 KiB まで、NUL を含まない（`summary_too_long`、`summary_invalid`）。本文全体の上限（64 KiB）が先に効く |
| `details_url` | 省略可。http / https、ホストあり、ユーザー名・パスワードなし、空白・制御文字なし、正規化した形で 2 KiB まで（`details_url_invalid`）。保存は正規化した形（例: スキームとホストは小文字） |
| `annotations[]` | 50 個まで（`too_many_annotations`）。報告ごとに **まるごと置き換える** |
| `annotations[].path` | リポジトリの中の相対パス: 1〜4096 バイト、`/` で始まらない、ドライブ文字（`C:`）で始まらない、`..` の区切りを含まない、`\` と制御文字を含まない（`annotation_path_invalid`） |
| `annotations[].start_line` / `end_line` | 1 以上、`end_line` は省略すると `start_line`、`start_line` 以上（`annotation_lines_invalid`） |
| `annotations[].level` | `notice` / `warning` / `failure`（`annotation_level_invalid`） |
| `annotations[].message` | 1 文字〜4 KiB（`annotation_message_invalid`） |
| `annotations[].title` | 省略可、255 バイトまで（`annotation_title_invalid`） |

- 各欄の上限は上限であって、すべてを同時に使える大きさではない（本文全体が 64 KiB まで。例: 注釈 50 個と要約を合わせて 64 KiB）。
- 1 つのコミットに持てる名前は **100 個まで**。101 個目の新しい名前は `FG-REVIEW-0021`（409、`reason=too_many_runs`）。既にある名前の報告は
  いつでも更新できる。名前は報告の種類ごとに固定する（`ci/gates-<実行番号>` のように変えない）。
- 答え: 新しい行は `201`、既にある行の更新は `200`。本文は下の「check run」1 つ。

## `GET /api/v1/projects/{id}/commits/{sha}/checks`（`gstn checks list`）

```json
{
  "schema_version": 1,
  "head_sha": "0123456789abcdef0123456789abcdef01234567",
  "check_runs": [ { "...": "check run（名前の順）" } ]
}
```

届かないコミット・存在しないコミット・形の正しくない `{sha}` は `FG-REPO-0023`（404）。

## check run の欄

| 欄 | 型 | 意味 |
|---|---|---|
| `id` | 整数 | 行の id（同じ名前の報告では変わらない） |
| `name` | 文字列 | `<名前空間>/<名前>` |
| `head_sha` | 文字列 | 報告されたコミット |
| `status` | 文字列 | `queued` / `in_progress` / `completed` |
| `conclusion` | 文字列 / null | `completed` のときだけ |
| `summary` | 文字列 | 書かれたまま（サーバーは描画しない） |
| `details_url` | 文字列 / null | 正規化した URL |
| `annotations[]` | 配列 | `{ "path", "start_line", "end_line", "level", "message", "title" }`（`title` は null 可） |
| `reported_by` | オブジェクト / `null` | 最後に報告した主体 `{ "id", "username" }`。`forge/` の run は `null`（forge 自身の worker。X2） |
| `revision` | 整数 | 報告の回数（作成で 1） |
| `created_at` / `updated_at` / `completed_at` | 文字列 / null | UTC、RFC 3339、マイクロ秒まで。`completed_at` はその結論になった最初の `completed` の報告の時刻（同じ結論の報告を繰り返しても変わらない） |

## 名前空間の割り当て（管理者、人だけ。`CheckNamespaceGrantManage`）

割り当ては **サービスアカウント × プロジェクト × 名前空間**（レビューの修正 1）。全プロジェクトに効く割り当ては作らない
（どのプロジェクトの Maintainer でも、`ci` を持つ別のチームのサービスアカウントを Developer に足せば、外せない必須のチェックを満たせて
しまうため）。

- `PUT /api/v1/admin/service-accounts/{id}/check-grants/{project_id}/{namespace}`（`gstn admin service-account grant-checks <id> --project <p> <ns>`）
- `DELETE …/check-grants/{project_id}/{namespace}`（`gstn admin service-account revoke-checks <id> --project <p> <ns>`）
- `GET /api/v1/admin/service-accounts/{id}/check-grants`（`gstn admin service-account check-grants <id>`、`ServiceAccountRead`）

`{namespace}` は `ci` か `kanflow` だけ（それ以外は `FG-REVIEW-0020 reason=namespace_not_grantable`）。人のアカウントには割り当てられない
（サービスアカウントでない id は `FG-REPO-0005`、DB のガード `check_namespace_grants_service_account` も拒む）。存在しないプロジェクトは
`FG-REPO-0005`、削除待ちのプロジェクトへの割り当ては `FG-REPO-0006`（取り消しはできる）。答えは `{ "grants": [{ "project_id", "namespace" }], "changed": 真偽 }`（`changed: false` は既にそうだった）。監査は
`check_namespace.granted` / `.revoked`（**プロジェクトに**。`details` は `user_id`・`project_id`・`namespace`）。プロジェクトの完全削除で
その割り当ても消える。割り当てとは別に、サービスアカウントはそのプロジェクトの Developer 以上のメンバーでなければ報告できない。

保護ブランチのマージの条件の答え（`PUT …/merge-rule`）は、必須のチェックのうち **そのプロジェクトについて報告できる有効なサービスアカウントが 1 つも無い** 名前を
`unreportable_checks: [...]` に並べ、そのとき `warnings: ["required_checks_unreportable"]` を付ける（`forge/` は、forge 自身が書く名前（`forge/spec-drift`・`forge/risk`・`forge/spec-lint`・`forge/spec-trace`・`forge/doc-lint`）を除いて常にここに入る）。
対処は runbook `checks-reporting`。

## 保持

実行と注釈は、`forge worker` のタスク `check_run_retention`（1 時間ごと）が消す: `updated_at` が `review.check_run_retention_days`
（既定 180 日、30〜3650）より古く、**開いている MR の今の head のコミットでない** もの（注釈は実行と一緒に、1 回に最大 1 000 件、古い順）。
消した批ごとにプロジェクトの監査に `check_run.retention`（個数だけ）が残る。プロジェクトの完全削除でも消える。消えた実行は
「報告なし」と同じに読まれる（`required_check_missing`）ので、必要なら報告し直す。runbook `checks-reporting` の E。

## forge 自身のチェック（X2、X7）

| 名前 | いつ | 結論 |
|---|---|---|
| `forge/spec-drift` | MR の版ごと（`merge_request.pushed`、開いたときの最初の版を含む）。その head のコミットに 1 回 | `success`: `.forge/repo.yml` が無い、または固定したすべての版が同じ系列の最新・`track:`・版の形でない。`neutral`: 新しい版がある（`FG-REPO-0027`）、版が無い（`FG-REPO-0025`）、参照先をサインインした全員が読めない・無い（`FG-REPO-0026`）、タグを数えきれない、ファイルを使えない（`FG-REPO-0024`）。`error`: 評価がやり直しても失敗した（要約に FG コードと request id） |

- 参照先は **サインインした全員** の権限で辿る（内部公開のプロジェクトだけ）。run はプロジェクトを読める全員に見えるので、一人でも読めない参照先の版は出さない。自分の権限での確認は `gstn browse spec-refs`。
- 版はタグ。`v1.10` > `v1.9`、`v1.5` = `v1.5.0`、接頭辞（`v`、`spec-v`）ごとの系列で比べ、`v2.0-rc1` のような付き物のタグは版にしない。
- 要約と注釈（`.forge/repo.yml` の行）に行ごとの状態を出す。参照先のタグの名前と新しい版の数は出さない（run はサービスアカウントにも見えるため。最新の版は自分の権限で `gstn browse refs` / `gstn browse spec-refs`）。版の形でない固定は notice の注釈。`reported_by` は `null`。
- forge が書く名前は 1 コミット 100 名の上限（`FG-REVIEW-0021`）に数えない。ほかの報告者が 100 件書いても forge の run は書ける（一覧は最大 102 件）。
- 評価が締め切り（30 秒）までに終わらない・やり直しても失敗したときは `error`。worker が落ちて答えられなかったジョブも、1 分ごとの掃除が `error` にする。

### `forge/risk`（X7、計画 04 M-6 の層 1。決定的な規則）

| 名前 | いつ | 結論 |
|---|---|---|
| `forge/risk` | MR の版ごと。その head のコミットに 1 回。head を版に持つ **開いている MR の target ごと** に、merge base から head までの差分（改名は 1 件）を調べ、最悪を採る | `success`: 見つからない。`neutral`: `warn` だけ（`FG-REVIEW-0024`）、または一部を調べられなかった（`FG-REVIEW-0025`: 開いている MR が無い・target が無い・履歴が共通でない・パスが UTF-8 でない・読めなかったファイルがある）。`failure`: `stop` の規則に当たった（`FG-REVIEW-0023`）。`error`: 評価がやり直しても失敗した |

- `stop`（どの方式の自動マージでも止める。M4 の自動マージは `failure` の head をマージしない）: 適用済みの migration の変更・削除・改名、migration の `DROP TABLE|COLUMN|SCHEMA|DATABASE`・`TRUNCATE`・`WHERE` のない `DELETE` / `UPDATE`・列の型の変更・`RENAME`・制約の削除・`DISABLE TRIGGER`、CI・デプロイのファイルの `migrate revert` など、認証情報のファイル名（`.env`、`id_rsa`、`.p12` など）、秘密鍵・トークンらしき本文（push の検査 X8 と同じ規則）、backup / restore / retention のジョブ、大量削除（削除 500 行以上かつ target の text 行の 30 % 以上、または削除 20 ファイル以上かつ 30 % 以上）、5000 ファイルを超える変更。
- `warn`（進む。警告が残る）: 新しい migration、`DROP INDEX` など、`GRANT` / `REVOKE`、認証・権限のコード（パスの語）、CI・デプロイ・インフラの設定、依存の定義・ロックファイル、シンボリックリンク・サブモジュール。
- 要約と注釈には **規則の id・パス・行・固定の理由・コード** だけを出す。一致した文字列（秘密、表名）は書かない。注釈は最悪のものから 50 件まで。`reported_by` は `null`。
- 本文を読むのは 60 ファイル、1 ファイル 256 KiB まで。残りの数は `content_not_scanned`（`neutral`）に出す。評価の締め切りは 45 秒。
- 必須のチェックにすると `neutral` も `failure` も満たさない（ADR-0024 §10）。必須にしなければ手動のマージは止めない。ローカル AI の層（層 2）と `.forge/risk.yml` は含まない（ADR-0023 Amendment X7）。
- 1 コミットの一覧は最大 105 件（報告者 100 + forge の名前 5。X4 で 2 つ、計画 13 段 2 で 1 つ増えた）。

### `forge/spec-lint` と `forge/spec-trace`（X4）

| 名前 | いつ | 結論 |
|---|---|---|
| `forge/spec-lint` | MR の版ごと（`merge_request.pushed`）と、作業項目の結びつきが変わったとき（`work_item.linked` / `unlinked`）・対象ブランチを変えたとき（`merge_request.updated` の `target_branch`）。後の 2 つはその MR の今の head。同じ head のジョブが待っている間は積まない。1 つのジョブ `check.spec_index` が `forge/doc-lint` を含めて 3 つの run を書く | `success`: 自分の仕様（`.forge/repo.yml` の `path:`、無ければ `spec/`）の文書と、自分の系列の `@req` が規則を守る。`failure`: 規則違反（`FG-REPO-0028`、要約と注釈に `パス:行 規則`）。`neutral`: 違反は見つからないが索引が上限に達した（`FG-REPO-0031`）。`error`: 評価がやり直しても失敗した |
| `forge/spec-trace` | 同上 | **助言**（計画 03 §9-4）。`success`: MR が触れる要件（マージベースからの追加・変更、追加した `@req`、結びついた作業項目）にすべて `@req` のテストがある。`neutral`: テストの無い要件・作業項目（`FG-REPO-0029`）、知らない ID・形の崩れたタグ（`FG-REPO-0030`）、索引が上限（`FG-REPO-0031`）。**`failure` にはしない** |

- 比べる相手は head と対象ブランチの先端の **マージベース**（無ければ、head のすべてを新しいものとして数える）。
- 仕様の文書（自分の仕様のディレクトリの下の Markdown）の中の `@req` はテストに数えない（説明やコード例のため）。
- `@req <要件 ID>` と `@req PBI-<番号>/AC-<番号>`（kanflow の受入基準）を受け付ける。結びついた作業項目 `PBI-<番号>` は、その受入基準のどれかのテストがあればよい。
- 要約と注釈に出すのは、このリポジトリ自身の文字（自分の仕様の ID とパス、自分のタグ、MR 自身の作業項目）だけ。参照先の仕様は読まない（`repo:` の `scope:` に当たる ID は「知っている」と数える）。参照先を含めた表は、自分の権限で `gstn spec trace`。
- M3 ではテストの **有無** だけを見る（パイプラインの結果は M4 の CI から。ADR-0017 §4.1.1）。
- 上限: 仕様の Markdown 1000 ファイル、合計 8 MiB、1 ファイル 256 KiB（超えると `file_too_large`）、`@req` の `git grep` の出力 8 MiB。評価の締め切り 60 秒、書き込み 8 秒ずつ（3 つの run と 3 つの `error` で 108 秒。ジョブの 120 秒に 12 秒の余裕）。落ちたジョブは 1 分ごとの掃除（`spec_index_sweep`）が 3 つの run を `error` にする。
- forge が書く名前（`forge/spec-drift`・`forge/risk`・`forge/spec-lint`・`forge/spec-trace`・`forge/doc-lint`）は 100 名の上限に数えない（一覧は最大 105 件）。

### `forge/doc-lint`（計画 13 段 2、ADR-0026）

| 名前 | いつ | 結論 |
|---|---|---|
| `forge/doc-lint` | `forge/spec-lint` と同じ（同じジョブ `check.spec_index` が、同じ head の **1 回の木の一覧と読み取り**から書く。新しいジョブの種類は無い） | `success`: 文書（前付けに `doctype` を書いた `.md`、どのフォルダでも）が型（`.forge/doctypes/<型>.yml`）と他の文書に照らして規則を守る（文書が無ければ `success`）。`failure`: 規則違反（`FG-DOC-0001`、要約と注釈に `パス:行 規則 — 対象 [コード]`）、型の定義が使えない（`FG-DOC-0003`）。`neutral`: 違反は見つからないが上限に達した（`FG-DOC-0002`。上限のときは壊れた参照だけなら `neutral`）。`error`: 評価がやり直しても失敗した |

- 検査は `gstn doc lint`（手元）と同じ `forge-doc` の lint を、同じバイトに対して行う（答えが一致する）。
- 壊れた参照（`ref_broken`）は誤り（計画 13 Q13）。移行の間は、リポジトリの設定（下の「移行中の警告」）で期限まで **警告** に下げられる。
  手元では `gstn doc lint --migration-warnings`（手元は DB を読めない。CI の答えが正）。

#### 移行中の警告（計画 13 Q13、ADR-0026 の「決まったこと」）

| 方法とパス | Action | 内容 |
|---|---|---|
| `GET /api/v1/projects/{id}/doc-lint/migration` | `ProjectRead`（`read_api`） | 設定（無ければ `warn_until: null`）、`in_force`（期限が今日以降）、`set_by` / `set_at`、サーバーの DB の日付 `today`（UTC）と、新しく決められる最後の日 `max_until`（today + 180 日）、`viewer.can_manage`（書き込みの試しの `authorize()`、目安だけ） |
| `PUT …/doc-lint/migration` | `DocLintMigrationManage`（人だけ、Maintainer 以上、`api`、代理実行は不可） | 本文 `{"warn_until": "YYYY-MM-DD"}`（必須）。`200` で設定を返す。暦の日付でない: `400 FG-WEB-0006 param=warn_until`。今日以前か 180 日より先: `400 FG-DOC-0005`（`reason=not_in_future` / `too_far`、ログに `today`・`max_until`） |
| `DELETE …/doc-lint/migration` | `DocLintMigrationManage` | `204`。設定が無ければ `404 FG-REPO-0005` |

- **日付は DB の時計で比べる**: 「今日」は `(now() AT TIME ZONE 'UTC')::date`。Web サーバーや利用者の PC の時計は使わない。
  期限の日（UTC）の終わりまで警告、翌日からは何も書かずに誤りへ戻る（期限の過ぎた行は残り、`in_force: false` と見える）。
- 期限の上限は **180 日**（`forge_core::doc_lint_migration::MAX_DAYS`。表の CHECK `doc_lint_migrations_until` が裏付け）。延ばすときは
  新しい日付を設定し直す（そのたびに監査の行が残る）。
- 下げている間、`forge/doc-lint` の要約の 1 行目は `migrating: until YYYY-MM-DD — …`。`ref_broken` の注釈は `warning`、要約の行に
  `(warning: migrating)`。壊れた参照**だけ**なら `success`（上限に達していれば `neutral`）。他の規則の違反と、ファイルごとの上限を超えて
  並べなかった問題は誤りのまま（`failure`）。
- 変更は監査の行 1 つ（プロジェクトに対して `doc_lint_migration.set`（`warn_until`・`previous`）/ `doc_lint_migration.cleared`、イベントなし）。
  表は `forge.doc_lint_migrations`（マイグレーション 20261203100000、`audited`）。プロジェクトの削除（purge）で消える。
- `gstn doc migration show|set|clear --project <id>`。画面はプロジェクトの設定の「移行中の文書の検査」。
- 上限: 木の一覧 50,000 件、`.md` 2,000 個、解析の上限（1 MiB）を超える `.md` 100 個（それぞれ上限＋1 バイトだけ読む。`gstn` と同じ。`doctype` の無い大きな Markdown は上限に当たらない。超えると `doc_large_files`）、合計 32 MiB、型の定義 64 個。最初に当たった上限を要約に書く。

## 仕様の API（X4）

`GET /api/v1/projects/{id}/specs/{requirements,versions,diff,trace}`（Action `SpecRead`、`read_api`、`RepoBrowse` と同じ判定）。
自分の仕様の索引を、読む人の権限で返す。`trace` は `.forge/repo.yml` の `repo:` を読む人の権限で辿り（読めないものは `unavailable`、`FG-REPO-0026`）、
その `scope:` の要件ごとに、このリポジトリの `@req` のテストを数える。上限に達した索引は `incomplete {error_code: FG-REPO-0031, reason}`。
1 人 1 分に索引 120 回分まで（`requirements`・`versions` は 1、`diff` は 2、`trace` は 1 + 参照の数。`FG-WEB-0010`）、同時に 2 本まで（`reason=in_flight`）、1 回 30 秒まで（`FG-WEB-0002 operation=spec_index`）。`gstn spec requirements|versions|diff|trace`。
