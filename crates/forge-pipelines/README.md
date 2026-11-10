# forge-pipelines

CI のパイプライン（計画 14 C2）。push・MR の版・手動の実行を、そのコミットの `.forge-ci.yml` から計画した **1 つのパイプラインとジョブ** にする。
計画は純粋な `forge-ci`、書き込みは `forge_db::pipelines`（1 つのトランザクション）。機能クレートで、他の機能クレートには依存しない（契約 9）。

| 公開の API | 中身 |
|---|---|
| `Pipelines::new(db, browse, marks)` | `forge serve`（手動の実行・読み取り）と `forge worker`（ジョブ `ci.pipeline_create`）が作る |
| `plan_and_record(&PipelineRequest, &RequestId) -> Outcome` | 下の手順。`NoDefinition`（ファイルが無い: 行を作らない）、`ProjectGone`、`Recorded { created, status, jobs }`（`created.existing`: 再配信で既にあった） |
| `fail(&PipelineRequest, &RequestId, &cause)` | ジョブの最後の試行の失敗: `error`（FG-CI-0002）の行を書き、warn で `cause_code` を 1 回記録する |
| `list` / `get`（`PipelineRead`）、`cancel`（`PipelineCancel`）、`retry`（`PipelineRetry`）、`run_manual`（`PipelineCreate`） | ルートの proof の上で。`cancel` / `retry` は変更の後のパイプラインを返す（`None`: そのプロジェクトに無い） |
| `MarkMeter` | コミットの機密の印を測る（呼ぶ側が実装する。forge-cli は `forge_checks::confidential` で、FG-AUTH-0022 と同じ規則） |
| `branch_patterns(&ProtectionRules)` | 保護ブランチのパターン（`forge/ci-lint` と同じ写像。機能クレートどうしは依存しないので重複している） |

## 計画の手順（`plan_and_record`）

1. プロジェクト（無ければ何もしない）と準備のできたリポジトリ（準備中は FG-REPO-0016: ジョブが再試行する）
2. `.forge-ci.yml` を browse API で読む（64 KiB まで、browse の時間切れ）。**無ければパイプラインを作らない**。ディレクトリ・大きすぎは `invalid`
3. 保護ブランチ（`forge_db::protected_refs::rules_for`）を `LintContext::with_protected_branches` に渡す（手元の検査の「何も保護されていない」は使わない）
4. `forge_ci::check` → `forge_ci::plan`。`Trigger.protected` は **保護されたブランチへの push だけ** 真。MR のパイプラインと手動の実行は偽なので、
   シークレットを持つジョブが選ばれると `invalid`（`secret_on_unprotected_ref`、黙って外さない）
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
- 取り消し・再実行・手動の実行は派生の状態で監査の行を書かない（構造化ログ `forge.ci.pipeline_*` に人と `request_id`）。

## 依存の向き

`forge-web / forge-cli → forge-pipelines → forge-auth / forge-git / forge-db → forge-core → forge-ci → forge-errors`
（`forge-hosting/tests/dependency_direction.rs`）。
