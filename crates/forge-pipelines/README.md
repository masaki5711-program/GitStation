# forge-pipelines

CI のパイプライン（計画 14 C2）。push・MR の版・手動の実行を、そのコミットの `.forge-ci.yml` から計画した **1 つのパイプラインとジョブ** にする。
計画は純粋な `forge-ci`、書き込みは `forge_db::pipelines`（1 つのトランザクション）。機能クレートで、他の機能クレートには依存しない（契約 9）。

| 公開の API | 中身 |
|---|---|
| `Pipelines::new(db, browse, marks)` | `forge serve`（手動の実行・読み取り）と `forge worker`（ジョブ `ci.pipeline_create`）が作る |
| `plan_and_record(&PipelineRequest, &RequestId) -> Outcome` | 下の手順。`NoDefinition`（ファイルが無い: 行を作らない）、`ProjectGone`、`Recorded { created, status, jobs }`（`created.existing`: 再配信で既にあった） |
| `fail(&PipelineRequest, &RequestId, &cause) -> Result<(), ForgeError>` | ジョブの最後の試行の失敗: `error`（FG-CI-0002）の行を書き、warn で `cause_code` を 1 回記録する。**行を書けなければその誤りを返す**（error で記録。ジョブはそのコードで再試行し、尽きれば dead になって doctor `ci.pipelines` / `jobs.dead` に出る。黙って終わらない） |
| `list` / `get`（`PipelineRead`） | ルートの proof の上で |
| `cancel`（`PipelineCancel`）、`retry`（`PipelineRetry`）、`run_manual`（`PipelineCreate`） | `ChangeRequest`（`request_id` と資格情報の id）とルートの proof を受け、**変更のトランザクションの中で判定をやり直し**（ロックした事実で `authorize()`）、監査の行を同じトランザクションで書く。変更の後のパイプラインを返す（無ければ FG-REPO-0005 `resource=pipeline`） |
| `MarkMeter` | コミットの機密の印を測る（呼ぶ側が実装する。forge-cli は `forge_checks::confidential` で、FG-AUTH-0022 と同じ規則） |
| `branch_patterns(&ProtectionRules)` | 保護ブランチのパターン（`forge/ci-lint` と同じ写像。機能クレートどうしは依存しないので重複している） |

## 計画の手順（`plan_and_record`）

1. プロジェクト（無ければ何もしない）と準備のできたリポジトリ（準備中は FG-REPO-0016: ジョブが再試行する）
2. `.forge-ci.yml` を browse API で読む（64 KiB まで、browse の時間切れ）。**無ければパイプラインを作らない**。ディレクトリ・大きすぎは `invalid`
3. 保護ブランチ（`forge_db::protected_refs::rules_for`）を `LintContext::with_protected_branches` に渡す（手元の検査の「何も保護されていない」は使わない）
4. `forge_ci::check` → `forge_ci::plan`。`Trigger.protected` は **保護されたブランチへの push だけ** 真。MR のパイプラインと手動の実行は偽なので、
   シークレットを持つジョブが選ばれると `invalid`（`secret_on_unprotected_ref`、黙って外さない）。同じ答えを行の `protected` に保存する（下の「シークレット」）
5. 機密の印（`MarkMeter`、ADR-0029 決定 5）: `open` / `confidential` / `undecided`（測れない・境界に当たった）
6. 1 回の書き込み: `pending`（ジョブあり。`needs` の無いジョブは `pending`、あるジョブは `created`）、`skipped`（選ばれたジョブが無い）、`invalid`（FG-CI-0001 と最大 20 件の問題）

一度だけ: push と MR のパイプラインは (project, source, ref, sha, MR) で 1 つ（`ci_pipelines_once`）。ジョブの重複キーも同じ同一性。手動の実行は毎回新しい。

## 状態

| パイプライン | 意味 |
|---|---|
| `pending` → `running` → `succeeded` / `failed` | 実行は C3（Runner）から |
| `canceled` | `cancel`（`pending` / `running` から。未完了のジョブも `canceled`） |
| `skipped` / `invalid` / `error` | 走るジョブが無い / 定義の誤り（FG-CI-0001）/ 読めなかった（FG-CI-0002） |

`retry` は `failed` / `canceled` だけ: 失敗・取り消しのジョブの `attempt` を 1 増やし、`pending` に戻す。ほかの状態は FG-CI-0003（409）。

## 監査

表 `ci_pipelines` / `ci_jobs` は派生の状態（ほとんどの書き込みは worker で、人がいない）なので、表にトリガーの監査は無い。
**人（と人の代わりのエージェント・サービスアカウント）の変更はコマンドが監査する**: `forge_db::pipelines` の `*_decided` が `ChangeTx` で
パイプラインの変更と `forge.audit_log` の行を 1 つのトランザクションで書く。

| action | いつ | details |
|---|---|---|
| `pipeline.canceled` | `cancel` | `project_id`、`pipeline_id`、`jobs`（取り消したジョブの数） |
| `pipeline.retried` | `retry` | `project_id`、`pipeline_id`、`jobs`（やり直すジョブの数） |
| `pipeline.run_manual` | `run_manual`（行を書いたとき。503 のときは書かない） | `project_id`、`pipeline_id`、`ref`、`sha`、`status` |

`resource_type = 'project'`、行為者はユーザーまたはサービスアカウント（エージェントのトークンはその人 + `credential_id` と実効スコープ）。
push・MR の版から作られたパイプラインは人の操作ではないので行を書かない（元の push / MR の監査が既にある）。

## シークレット（後の MR への約束）

`ci_pipelines.protected` は **保護されたブランチへの push** のときだけ真（表の CHECK `ci_pipelines_protected` が push 以外の真を拒む）。
ジョブにシークレットを渡してよいかは `forge_db::pipelines::StoredPipeline::secrets_allowed()`（`source == push && protected`）**だけ** で決める
（ADR-0029「後の MR の義務」5）。実行の時点の保護の設定から判定し直さない。再実行は 2 つの列を変えないので答えも変わらない。
試験: `only_a_protected_push_may_release_secrets`（forge-db）、`only_a_push_to_a_protected_branch_is_recorded_as_protected`（ここ）。

## 見張りと保持

- `forge doctor` の `ci.pipelines`: 24 時間以内の `error` のパイプライン、7 日以内に dead になった `ci.pipeline_create` のジョブで fail（FG-CI-0002、
  runbook `ci-pipelines`）。24 時間動いていない `pending` / `running` は warn。
- 保持期間は 90 日（ログと同じ。計画 14 §4.1）。消すジョブは C5 で入れる。それまではプロジェクトの purge だけが消す。

## エラーコード

| コード | いつ |
|---|---|
| FG-CI-0001 CiConfigInvalid | 定義・計画の誤り（行に記録。ルートではなく行の `error_code`） |
| FG-CI-0002 PipelineCreateFailed | 定義・保護ブランチを読めなかった（ジョブの最後の試行で `error` の行、手動の実行は 503）。runbook `ci-pipelines` |
| FG-CI-0003 PipelineStateConflict | 取り消し・再実行ができない状態（409） |

## 後の MR

- C3: Runner がジョブを取り、状態とパイプラインの状態を進める。`runner_id` の外部キーは `ci_runners` と一緒に足す。
- C5: CI の出力の読み取りを `copy_mark` で判定する（`undecided` はエージェントに閉じる）。今のルートはメタデータだけ。
- C12: 手動・定期の実行の保護（今は保護されない扱い）。
- C3 / C6: シークレットは `secrets_allowed()` だけで判定する（上）。C11: 再実行されたデプロイのジョブも人の `DeployApprove` を待つ。
- C5: 90 日より古いパイプラインとジョブを消すジョブ。

## 依存の向き

`forge-web / forge-cli → forge-pipelines → forge-auth / forge-git / forge-db → forge-core → forge-ci → forge-errors`
（`forge-hosting/tests/dependency_direction.rs`）。
