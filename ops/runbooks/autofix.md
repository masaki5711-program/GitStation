# Runbook: autofix（FG-GIT-0037。関連: FG-AUTH-0024、FG-CI-0031、FG-REVIEW-0031、FG-GIT-0038）

M4 C14（ADR-0017 Amendment C14、計画 04 C-5）の決定的な自動修正（`forge serve` のタスク `autofix`、購読者 `autofix-triggers`、
表 `forge.project_autofix` / `forge.autofix_attempts`）が、拒否した・動かない・期待と違うとき。LLM の修正はまだない（M5）。
自動修正は **プロジェクトごとに既定で無効**。Maintainer がプロジェクトの設定の「自動修正」で有効にしたプロジェクトだけで動く。

流れ: MR のマージ結果の `ci/pipeline` が失敗、または MR が対象ブランチと衝突（`merge_request.conflict_detected`）
→ 試行が `queued` → worker が **git を読む前に** ボットの権限を判定（保護されていないソースブランチ、作成者と有効にした人が
Developer 以上）→ 対象ブランチの `.forge-ci.yml` の `autofix.fixers` から修正を選ぶ → 入力のコミット（head の tree か、サーバーが
計算した衝突のマージ。`.forge-ci.yml` は対象ブランチのもの）で修正だけのパイプライン（source `autofix`、ジョブ 1 つ、シークレットなし）
→ Runner が作業ツリーを bundle（`autofix.bundle`）で返す → サーバーが取り込み（fsck、前提は入力のコミットだけ）、tree だけを使い、
範囲を検査し、`commit-tree` でボットのコミットを作り、ソースブランチを比較して交換で進める。

## Trigger

- `forge serve` のログに `FG-GIT-0037` AutofixCandidateInvalid（error、`forge.autofix.refused`）: Runner の出力（bundle）か、
  その tree が規則に反した。`reason`（`precondition_mismatch`、`bundle_rejected`、`artifact_digest`、`outside_fixer_paths`、
  `forbidden_path`、`ci_file`、`outside_conflict_hunk`、`secret_in_output` など）と `paths`。**Runner の不具合か侵害の兆候**。
- `forge.autofix.denied`（warn、`FG-AUTH-0024`）が続く: ボットがブランチに書けない（`cause_reason`）。
- `forge.autofix.failed`（warn、`FG-CI-0031`）が続く: 修正のジョブが失敗している。
- 利用者から「自動修正が動かない」「提案だけで止まった」と問い合わせがあった。

## Impact

- 自動修正が積まれないだけ。MR は人が直せば進む。人のコミットを上書きすることはない（比較して交換、force push なし）。
- `FG-GIT-0037` の `precondition_mismatch` / `bundle_*` / `artifact_digest` は Runner の侵害の疑い: Runbook `ci-runner` と
  あわせて SEV3 以上で扱う。`secret_in_output` は秘密情報の流出の疑い（Runbook `ci-secrets`）。
- それ以外の拒否（`outside_fixer_paths` など）は修正の定義の誤りで SEV4。

## Diagnose（読み取りのみ）

1. MR の 1 行: `GET /api/v1/projects/{id}/merge-requests/{iid}/autofix`（MR の画面の「自動修正:」の行）。
   `state`（`queued` / `running` / `applied` / `no_change` / `refused` / `proposed` / `failed` / `skipped` / `superseded`）、
   `reason`、`paths`、`commit_sha`、`error_code`、`request_id`。
   - `skipped reason=disabled`: プロジェクトの自動修正が無効（既定）。`no_fixer`: 対象ブランチの `.forge-ci.yml` に使える修正が
     ない（パイプラインの失敗には `format` / `lint`、衝突には衝突したパスをすべて `paths` に含む `regenerate`）。`no_fix`: 登録した
     修正をすべて試して直らなかった。`max_attempts`: その MR で 24 時間に 6 回を超えた。`author_not_a_person`: サービスアカウント・
     ボットの MR（既定で対象外）。`no_definition` / `definition_invalid`: 対象ブランチの `.forge-ci.yml` がない・不正。
   - `skipped reason=denied`（`FG-AUTH-0024`）: ログの `cause_reason`（`autofix_protected_branch`、`autofix_author_role`、
     `autofix_enabler_role` など）。
   - `proposed`（`FG-REVIEW-0031`）: マイグレーション・バイナリ・保護されたパスに触れた。積まない（決まり）。
   - `superseded`（`FG-GIT-0038` など）: 途中で head が変わった・MR が閉じた・衝突がなくなった。新しい head で改めて動く。
2. プロジェクトの設定: `GET /api/v1/projects/{id}/autofix`（`enabled`、`changed_by` = 有効にした人）。
3. ログを `request_id` と `attempt_id` で読む: `forge.autofix.started`（`fixer`、`input_sha`、`pipeline_id`、`job_id`）、
   `forge.autofix.applied` / `refused` / `proposed` / `failed` / `denied` / `superseded` / `ended`。
4. 修正のジョブのログ: パイプラインの画面（source `autofix` のパイプライン）か `gstn ci log`。Runner の側の手順は
   `step=autofix_*` の FG-CI-0009 で出る（`autofix_fetch`、`autofix_add`、`autofix_bundle` など）。
5. 表を読む:
   `SELECT project_id, enabled, changed_by, changed_at FROM forge.project_autofix WHERE project_id = <id>;`
   `SELECT id, trigger, fixer, state, reason, detail, error_code, head_sha, target_sha, input_sha, pipeline_id, job_id, commit_sha, request_id, created_at, finished_at FROM forge.autofix_attempts WHERE merge_request_id = <mr_id> ORDER BY id DESC LIMIT 20;`
   `SELECT id, runner_id, status, attempt FROM forge.ci_jobs WHERE id = <job_id>;`
   `SELECT id, path, size_bytes, encode(sha256, 'hex') FROM forge.ci_artifacts WHERE job_id = <job_id>;`
   `queued` / `running` が何十分も動かないなら worker が止まっている（`forge.serve.autofix_started` のあと `autofix_tick` の失敗）
   か、Runner が修正のジョブを取っていない（Runbook `ci-runner`、FG-CI-0005）。
6. 監査: `autofix.settings_changed`（誰がいつ有効・無効にした）、`merge_request.autofix_finished`（actor system、試行ごと）、
   ボットのコミットの `repository.pushed`（actor system、label `forge-autofix`）。

## Mitigate（変更は人の承認のあと）

- **すぐ止める**: プロジェクトの設定の「自動修正を無効にする」（`PUT /api/v1/projects/{id}/autofix` `{"enabled": false}`、
  Maintainer 以上）。新しい試行は `skipped reason=disabled` になり、走っている試行も着地の前の判定で拒否される。DB を直接書き換えない。
- `FG-GIT-0037` の `precondition_mismatch` / `bundle_*` / `artifact_digest`: その Runner を無効にし（`forge admin runner update
  --disable`、Runbook `ci-runner`）、Runner の作業ディレクトリと成果物のファイル（`<storage.root>/ci-artifacts/<project>/<id>`）を
  保全してから調べる。ブランチには何も書いていない。取り込みと検査は隔離領域（`<repo>/objects/forge-autofix-<uuid>`）で行い、拒否すれば
  消すので、本体のリポジトリにオブジェクトは入っていない（消せなかったときは warn `forge.autofix.quarantine_left`: そのディレクトリを消す）。
- `secret_in_output`: 修正が秘密情報らしい文字列を書いた。何も積んでいないが、Runner のログと成果物を Runbook `ci-secrets` に
  従って扱う。
- `outside_fixer_paths` / `forbidden_path` / `ci_file`: 修正の定義（対象ブランチの `autofix.fixers.<name>.paths`）を直す。
  固定の拒否（`.forge-ci.yml`、`Cargo.toml`、`.claude/**` など、ADR-0017 §2.1）は設定で緩められない。
- `FG-AUTH-0024 autofix_enabler_role`: 有効にした人が権限を失った。Maintainer が自動修正を無効にしてから有効にし直す（その人が
  新しい enabler になる）。`autofix_protected_branch` は決まり（保護されたブランチには書かない）。
- `FG-CI-0031`: 修正のジョブのログを読み、対象ブランチの修正（イメージ、スクリプト、時間）か Runner を直す。

## Verify

- 次の失敗（または衝突）で `GET …/autofix` が `running` → `applied` になり、ソースブランチの先頭が `commit_sha`（作者
  `forge-autofix <autofix@forge.invalid>`、トレーラー `Forge-Autofix-Attempt`）になる。新しいパイプラインが走る。
- ボットのコミットでは承認が引き継がれない（MR の承認がやり直しになる）。
- `forge.autofix.refused` が止まる。

## 関連

- ADR-0017 §1 / §1.2 / §2.1 / §2.5、Amendment C14。計画 04 C-5、計画 14 C14。
- Runbook `ci-runner`（Runner の停止と保全）、`ci-secrets`、`merge-request-mergeability`（衝突の判定）。
