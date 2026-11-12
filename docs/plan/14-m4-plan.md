# 14 M4 の計画 — CI/CD、Runner、マージ結果での検証、承認付きデプロイ（R1 の CI の判定）

作成: 2026-11-10（architect）。状態: **承認済み（2026-11-10）** — 利用者が「推奨で進めて」と指示し、§3 の推奨と §9 の決定をすべて採用した。§10 の 3 つは事実の情報が要るので、その MR に着手する前に聞く。
参照: 01 §3.4・§8（M4）、04（M-1〜M-6、P-1〜P-9、§5、§8）、11 §9・§10、ADR-0006・0013・0017・0019・0024 §10・0025・0026（2026-11-10 の決定）、`crates/forge-checks/README.md`、runbook `checks-reporting`。

## 0. 前提の点検

### 0.1 目標（1 文）

forge 自身の `main` の必須チェックを、GitStation の CI がマージ結果で出す `ci/pipeline` に置き換え、社内アプリ 1 本を stg へ自動、prod へ人の承認でデプロイし、機密の文書のあるリポジトリでも CI は通るが AI にはその出力を渡さないことを確かめる。

### 0.2 決定済み（議論し直さない）

| 日付 | 決定 |
|---|---|
| 2026-10-26 | 04 §8: マージ結果でのパイプラインが既定、マージキューを M4 に前倒し、stg は自動ロールバック・prod は人の判断 |
| 2026-11-03 | 自動マージの既定は `full`。止めるのはプログラムの破壊・DB の破壊・個人情報の流出だけ。prod のデプロイは人の承認（04 決定 8、ADR-0017 Amendment） |
| 2026-11-10 | **Runner は案 A**（§3.1）。**CI の主体は Runner と Job の 2 つの新しい資格情報**（§3.2、ADR-0029）。§9 の 8 項目を推奨で決定 |

### 0.3 コードで確かめたこと（`main`）

- `forge-ci`・`forge-deploy`・`forge-runner` のクレートは無い。`forge-errors/build.rs` の AREAS には `CI`・`DEPLOY` が既にあり、コードは 0 件。
- `Credential` は `Session`・`Token`・`ActAs` の 3 つ（`forge-auth/src/model.rs`）。代理は `act_as::decide` が `authorize()` の最初に判定する。この形を Job に使う。
- AI の複製の拒否は `RepoRead` × `Resource::RepoCopy` の 1 か所（`authorize.rs` 339-347）で、`is_agent_audience()`（`model.rs` 180）を使う。同じ関数を文脈 API の FG-AUTH-0021 も使う。
- Checks API の名前空間 `ci/` は割り当てたサービスアカウントが書く（runbook `checks-reporting`）。forge 自身のチェックは worker が HTTP を通らず DB の経路で書く（`record_forge_check_run`）。
- ジョブの取り出しは既に `FOR UPDATE SKIP LOCKED`（`forge-db` の webhooks・mergeability）。Runner のスケジューリングも同じ形にする。

## 1. M4 の範囲

R1 の CI の判定（§8）に要る MR に ★ を付ける。★ の無いものは R1 の後でもよい。

| 項目 | 区分 | 出典 |
|---|---|---|
| エージェントモデル（評価役 2 つ、契約 9 の文） | ★ | 04 §7、00 §8 |
| Job の主体、`.forge-ci.yml`、パイプライン、Runner の API と本体、`ci/pipeline` とログ | ★ | 01 §3.4、04 §5・P-1〜P-3・P-6 |
| マージ結果の ref と必須チェックの鮮度 | ★ | 04 M-1・M-2 |
| SSH のデプロイ、prod の承認、凍結期間、stg の自動ロールバック | ★ | 01 §3.4、04 §5 |
| 成果物・シークレット、マージキュー、自動マージ、一手の revert、定期・手動 | M4（R1 の外） | 04 M-3〜M-5 |
| SSH の git 転送 | M4 の終盤（R1 の外） | 01 §3.1 |
| LLM の診断（P-8）、不安定なテスト（P-10） | M5 | 04 §7 |

## 2. 依存の順

```
M4-0 [root] ─▶ ADR-0029 / C0（Job の主体）─▶ C1（定義）─▶ C2（パイプライン）─▶ C3（Runner の API）─▶ C4（forge-runner）
   └▶ C5（ci/pipeline・ログ・指紋）─▶ C7（マージ結果と鮮度）─▶ X（ci/gates からの切り替え）═▶ R1 の CI の判定
   └▶ C6（成果物・シークレット）─▶ C11（デプロイ。ADR-0031）
   後半: C8（キュー。ADR-0030）→ C9（自動マージ）→ C10（revert）、C12、C13
```

## 3. 設計の選択肢と推奨（決定済み）

### 3.1 Runner の形

| 軸 | **A: 自前の `forge-runner`（採用）** | B: gitlab-runner の API 互換 | C: forge のホストでジョブを動かす |
|---|---|---|---|
| 内容 | 別の Linux ホストの常駐プロセスが HTTPS の long-poll でジョブを取りに来る。割り当てはサーバーが PG の `SKIP LOCKED` で決める。ジョブごとに使い捨ての rootless Podman のコンテナ。Runner がするのは git の fetch・Podman・ログの送信だけ | 既存の gitlab-runner をそのまま使う | worker が子プロセスでジョブを動かす |
| 失敗の見え方 | すべて FG-CI のコード（取りこぼし・リースの喪失・時間切れ）。`request_id` と `job_id` で追える | 外部の実装の挙動とログに頼る。互換の API の差分が障害の原因になる | ジョブの暴走が forge 本体を止める |
| 安全 | Runner は外から接続を受けない。トークンは Runner とジョブで分ける | GitLab のトークンの意味をまねる必要があり、攻撃面が広い | 隔離が無い。**不採用** |
| 判断 | 契約 2・5 を自分で守れ、Claude がコードとログだけで診断できる | 互換の維持が永続の負担。**不採用** | **不採用** |

### 3.2 CI の主体（詳細は ADR-0029）

- `Credential::Runner`: Runner の API だけ。リポジトリは読めない。
- `Credential::Job`: ジョブごとの短命のトークン。1 つのプロジェクト、固定した SHA、ジョブの生存期間に結びつき、終わりで失効する。判定は新しい `job.rs`（`act_as::decide` と同じ形）。
- `is_agent_audience` は **変えない**（FG-AUTH-0021 が使う）。Job は自分のプロジェクトの固定した SHA を機密の印に関係なく複製できる。出力（ログ・成果物・診断）は元のリポジトリの印を継ぎ、先端に機密のあるリポジトリでは AI に FG-AUTH-0022。
- サービスアカウントは今と同じく拒否される。案 C2（AI が起こしたパイプラインのジョブをエージェントとして扱う）は不採用（ADR-0029）。

## 4. 設計の要点

### 4.1 データ（すべて追記のマイグレーション。`tables.toml` の台帳と doctor の期待値も更新）

| MR | 表 |
|---|---|
| C0 | `ci_job_tokens`（ハッシュだけ、`job_id`・`project_id`・`sha`・`expires_at`・`revoked_at`） |
| C2 | `ci_pipelines`（`project_id`・`ref`・`sha`・`source` = push / merge_request / schedule / manual、`triggered_by`、`copy_mark`、状態）、`ci_jobs`（`needs`、`timeout`、`runner_id`、`lease_until`、状態、試行の数） |
| C3 | `ci_runners`（トークンのハッシュ、タグ、有効、最後の連絡） |
| C5 | `ci_log_chunks`（ジョブ・番号・本文。追記だけ）、`failure_fingerprints`（指紋・ジョブ・最初と最後・直した MR） |
| C6 | `ci_artifacts`（ジョブ・パス・大きさ・ハッシュ・期限・`keep`）、`ci_variables`（暗号文・鍵の ID・保護ブランチだけ・マスク） |
| C7 | `merge_result_refs`（MR・head・target の先端・結果の SHA） |
| C8 | `merge_queue_entries`（ターゲット・順・バッチ・状態） |
| C11 | `deploy_environments`（dev / stg / prod、承認の規則、凍結期間、自動ロールバック）、`deploy_targets`（ホスト・ポート・ユーザー・known_hosts の指紋・スクリプトの名前）、`deployments`（SHA・承認者・状態・前の版） |

### 4.2 Action と権限の表（どれも allow と deny の行。`authz-matrix-auditor` が確かめる）

| Action | allow | deny（代表） |
|---|---|---|
| RunnerRegister / RunnerManage | 管理者の Web のセッション、人だけ | トークン、エージェント、サービスアカウント |
| RunnerJobRequest / Heartbeat / LogAppend / Complete | 有効な Runner、リースを持つジョブ | 無効な Runner、他の Runner のジョブ、終わったジョブ、Job・人・サービスアカウント |
| RepoRead × `PinnedCopy`（ジョブの fetch） | 自分のプロジェクトの固定した SHA（印に関係なく） | 他のプロジェクト、他の SHA、終わったジョブ |
| JobArtifactUpload / Download | 自分のジョブ / 同じパイプラインの `needs` の成果物 | 他のパイプライン、Runner |
| PipelineRead / CiLogRead / CiArtifactRead / PipelineDiagnosisRead | `RepoBrowse` と同じ。AI は出力の印が `Open` のときだけ | AI に機密の出力（FG-AUTH-0022）、Job、Runner |
| PipelineCreate（手動）/ Retry / Cancel | Developer 以上 | Reporter、Runner、Job |
| CiVariableManage | Maintainer 以上、人だけ | Developer、エージェント、サービスアカウント |
| DeployEnvironmentManage / DeployTargetManage | Maintainer 以上、人だけ（ターゲットは管理者） | Developer、エージェント |
| DeployApprove | stg: 規則どおり。**prod: Maintainer 以上の人の Web のセッションだけ、MR の作成者を除く** | エージェント、サービスアカウント、代理、自動マージの主体、トークン |
| MergeQueueManage / Pause | Maintainer 以上（停止は Developer 以上） | エージェント |

### 4.3 エラーコード（仮。番号は登録の順）

| 仮の名前 | 内容 | 重さ | runbook |
|---|---|---|---|
| CI-a CiConfigInvalid | `.forge-ci.yml` のスキーマ・DAG・ダイジェスト・時間切れの未設定 | info | — |
| CI-b JobLeaseLost | Runner の心拍が途絶え、リースを取り戻した | warn | `ci-runner` |
| CI-c JobTimedOut / CI-d JobCancelled | ジョブの時間切れ / 取り消し | info | `ci-runner` |
| CI-e NoRunnerAvailable | 割り当てられるジョブが一定時間残る | **alert** | `ci-runner` |
| CI-f ContainerStartFailed | Podman の起動・イメージの取得の失敗 | warn | `ci-runner` |
| CI-g SecretKeyUnavailable / CI-h SecretOnUnprotectedRef | 鍵のファイル / 保護されていない ref でのシークレット | alert / info | `ci-secrets` |
| CI-i RequiredCheckStale | 必須チェックが古いマージ結果のもの | info | — |
| DEPLOY-a DeployApprovalRequired / DEPLOY-b DeployFrozen | 承認待ち / 凍結期間 | info | `deploy` |
| DEPLOY-c DeployTargetUnreachable / DEPLOY-d HostKeyMismatch | SSH の接続 / known_hosts の不一致 | **alert** | `deploy` |
| DEPLOY-e HealthCheckFailed / DEPLOY-f RollbackFailed | ヘルスチェックの失敗 / ロールバックの失敗 | warn / **alert** | `deploy` |

認証の失敗は FG-AUTH-0001、拒否は FG-AUTH-0003（`reason` で区別）、AI への出力の拒否は FG-AUTH-0022、git は FG-GIT の既存のコード（ADR-0029）。

## 5. MR の分け方

置き場所: **[root]** = エージェントモデル（先に入れる）、印なし = GitStation。各 MR の終わりは計画 11 §5 と同じ（fmt・clippy・nextest、`gen-catalog`・`gen-authz` の差分なし、`/review` で CRITICAL / HIGH なし）。`gstn` のコマンドはルートと同じ MR。

| MR | 中身 | 権限の表 | コード | マイグレーション / 主な試験 | 評価役 | runbook / ADR |
|---|---|---|---|---|---|---|
| **M4-0★ [root]** `chore/harness-m4-ci-reviewers` | `ci-runner-security-reviewer`・`deploy-safety-reviewer` と eval、契約 9 に `forge-ci`・`forge-deploy`・`forge-runner`（`forge-runner` はサーバーのクレートに依存しない） | — | — | eval: 見逃しの再現 | `harness-optimizer` | changelog |
| **C0★** `feat/auth-ci-principals` | `Credential::Runner` / `Job`、`job.rs`、`PinnedCopy`、出力の印 | Runner・Job の全 Action の allow / deny、`job_denied_for_<action>` の生成 | AUTH の既存 | `ci_job_tokens` / 失効の直後の拒否、他 SHA の拒否、`is_agent_audience` が変わらない | `security-reviewer`、`authz-matrix-auditor`、`rust-reviewer` | ADR-0029 |
| **C1★** `feat/ci-config-lint-plan` | `forge-ci`: `.forge-ci.yml` の解析・lint・plan（純粋）、`forge/ci-lint`、`gstn ci lint/plan` | — | CI-a | — / 循環、存在しない `needs`、時間切れ、ダイジェスト、保護外のシークレット | `rust-reviewer`、`ci-runner-security-reviewer` | — |
| **C2★** `feat/ci-pipelines` | push と MR からパイプラインを作る（`repository.pushed` の購読者 → ジョブ）、印の測定、`gstn pipeline` | PipelineRead / Create / Retry / Cancel | CI-c・d | `ci_pipelines`・`ci_jobs` / イベントの取りこぼしなし、二重作成なし | `database-reviewer`、`authz-matrix-auditor`、`silent-failure-hunter` | — |
| **C3★** `feat/ci-runner-api` | 登録・long-poll・心拍とリース・ログ・取り消し、リースの回収（reaper） | Runner の Action | CI-b・e | `ci_runners` / `SKIP LOCKED` の同時取得、リース切れの回収、他の Runner のジョブ | `ci-runner-security-reviewer`、`database-reviewer`、`silent-failure-hunter` | `ci-runner` |
| **C4★** `feat/ci-forge-runner` | `forge-runner`: git は argv・`--`・時間切れ、Podman は argv・rootless・ネットワーク既定で閉じる・読み取り専用のルート | — | CI-f | — / コマンドの注入、時間切れでコンテナを消す、ジョブの間で状態を残さない | `ci-runner-security-reviewer`、`git-protocol-reviewer`、`rust-reviewer` | `ci-runner` |
| **C5★** `feat/ci-pipeline-checks-logs` | `ci/pipeline` を `Principal::system(Ci)` 相当（worker、DB の経路）で書く、ログの API（印を継ぐ）、失敗の指紋と診断の API（P-6・P-9 の決定的な部分）、`ci/pipeline` を HTTP から予約 | CiLogRead / DiagnosisRead、CheckWrite の `ci_namespace_internal` | AUTH-0022 の文面の拡張 | `ci_log_chunks`・`failure_fingerprints` / AI に機密のログを渡さない、マスク | `security-reviewer`、`error-contract-auditor`、`authz-matrix-auditor` | `checks-reporting` 改訂 |
| C6 `feat/ci-artifacts-secrets` | 成果物、シークレット（ADR-0013 の鍵のファイル、保護ブランチだけ、マスク） | JobArtifact*、CiArtifactRead、CiVariableManage | CI-g・h | `ci_artifacts`・`ci_variables` / MR のパイプラインに渡らない、ログのマスク | `security-reviewer`、`ci-runner-security-reviewer` | `ci-secrets` |
| **C7★** `feat/review-merge-result-staleness` | `refs/merge-requests/<iid>/merge` の作成、必須チェックの鮮度、保護ブランチで古いものを止める | — | CI-i | `merge_result_refs` / target が進むと古くなる、隠した ref が clone に出ない | `git-protocol-reviewer`、`rust-reviewer` | ADR-0024 Amendment |
| C8 `feat/review-merge-queue` | キュー（バッチ ≤4、失敗で二分）、停止 | MergeQueueManage / Pause | CI-next | `merge_queue_entries` / 二分の正しさ、マイグレーションの MR は 1 本ずつ | `database-reviewer`、`git-protocol-reviewer` | ADR-0030 |
| C9 `feat/review-auto-merge` | `merge.auto`（既定 `full`、04 M-5 の実効の段）、`stop` の 3 分類だけで止める | 自動マージの主体の行 | REVIEW-next | — / `stop` で止まる、緊急停止 | `security-reviewer`、`authz-matrix-auditor` | ADR-0017 Amendment |
| C10 `feat/review-one-step-revert` | 1 つのマージを 1 操作で戻す MR、main の失敗で下書き | 既存の MR の Action | — | — / マージコミット・squash の両方 | `git-protocol-reviewer` | — |
| **C11★** `feat/deploy-ssh` | `forge-deploy`: forge の worker から SSH（argv、`BatchMode=yes`、`ConnectTimeout`、固定した known_hosts、ターゲットごとのスクリプトだけ）、prod の承認、凍結期間、stg の自動ロールバック | DeployApprove ほか §4.2 | DEPLOY-a〜f | `deploy_*`・`deployments` / prod をエージェント・サービスアカウント・自動マージの主体が承認できない、鍵の不一致で止まる | `deploy-safety-reviewer`、`security-reviewer`、`authz-matrix-auditor` | `deploy`、ADR-0031 |
| C12 `feat/ci-schedules-manual` | 定期・手動のジョブ、仕様の公開（04 §6 `spec-publish`） | PipelineCreate の手動 | — | — | `rust-reviewer` | — |
| C13 `feat/git-ssh-transport` | SSH の git 転送（M4 の終盤、R1 の外） | RepoRead / RepoPush の SSH 経路 | GIT-next | — | `git-protocol-reviewer`、`security-reviewer` | `smart-http` 相当 |
| **C5s★** `feat/ci-job-services` | ジョブの `services`（PostgreSQL など、2 つまで）: `forge-ci` の lint、`forge-runner` のポッド（`--network=none`、ループバックだけを共有）、どの終わり方でもポッドを消す、失敗のときサービスのログの末尾 | — | CI-0014 | — / ポッドの argv、どの経路でもポッドを消す、起動時に残りを消す、本物の Podman で PostgreSQL に 127.0.0.1 で届き外へは出ない | `ci-runner-security-reviewer`、`rust-reviewer` | ADR-0029 追記、`ci-runner` |
| **X★** `chore/ops-ci-cutover` | `ci/gates` のスクリプトと `ci/pipeline` を並行し、5 回一致したら必須チェックを `ci/pipeline` に替え、`ci/` の割り当てを外す | — | — | — / 5 回の一致の記録 | `ci-runner-security-reviewer` | `checks-reporting` |

**C1 の注記（2026-11-10）**: 契約 9 を改め（エージェントモデル 813c66e）、`forge-ci` を純粋な層（`forge-core → forge-ci → forge-doc → forge-errors`）に置いた。
`gstn` は `forge-ci` に依存してよい（ADR-0020 の Amendment）。パイプラインの作成・実行（C2 以降、DB とイベント）は機能クレート（仮称 `forge-pipelines`）に置く。
`forge/ci-lint` は C1 では入れず後の MR に回した（`check.spec_index` のジョブに `.forge-ci.yml` の読み取りと保護ブランチの一覧の DB の読み取りを足す必要があり、
`forge/doc-lint` の機械的な写しにならないため）。

**C2 の注記（2026-11-10）**: `forge-pipelines`（機能クレート）、表 `ci_pipelines`・`ci_jobs`（派生の状態。人の取り消し・再実行・手動の実行は
`pipeline.canceled` / `.retried` / `.run_manual` の監査の行を同じトランザクションで書く）、購読者 `ci-pipelines`、ジョブ `ci.pipeline_create`、
FG-CI-0002 / 0003、doctor `ci.pipelines`、runbook `ci-pipelines`。一意キーは (project, source, ref, sha, MR)（同じ SHA を別のブランチへ進めた
push を落とさないため ref を含めた）。**保持期間**: パイプラインとジョブは 90 日（ログの保持の決定に合わせる）。消す処理は C5（ログと同じジョブ）で入れる。
**後の MR の義務**（ADR-0029 の 5〜7）: シークレットは `source = push` かつ作成時に保護のときだけ（C3・C6）、保護された push の再実行でも
デプロイは人の `DeployApprove`（C11）、手動・定期の保護は C12。

**C3 の注記（2026-11-10）**: 表 `ci_runners`（監査あり）・`ci_job_tokens`・`ci_log_chunks`（ログの保存を C5 から前倒し。読む経路は C5 まで無い:
試験 `ci_logs_have_no_read_path_before_c5`、読み取り用の接続は `body` を読めない）、`ci_jobs.lease_losses`。Action `RunnerRegister`・`RunnerManage`・
`RunnerJobRequest`・`RunnerJobHeartbeat`・`RunnerLogAppend`・`RunnerJobComplete`、FG-CI-0004 / 0005 / 0006、worker の `ci_lease_reaper`、
doctor `ci.runners`、runbook `ci-runner`。ジョブのスクリプトは表に持たず、Runner が固定した SHA の `.forge-ci.yml` を `definition_hash` で
確かめて読む（C4）。ジョブの fetch の Smart HTTP（`refs/pipelines/<id>/head`、`authorize_pinned_copy`）は C4 に回した（`git-protocol-reviewer` が見る MR）。
Runner の登録は管理者の Web のセッションだけ（`gstn` はトークンなので呼べない。画面は後の MR）。

**C4 の注記（2026-11-11）**: クレート `forge-runner`（バイナリ。依存は forge-core・forge-errors・forge-client だけ、forge-ci は
`forge_client::forge_ci` の再公開で使う）。git と Podman は `ProcessRunner` の trait の向こう（argv だけ、環境を消す、時間切れ、
停止の旗）で、試験は偽物で argv を確かめる。本物の Podman の試験は Runner のホストができるまで `#[ignore]`（§10.1）。
`.forge-ci.yml` は `LintContext::pinned_by_server`（シークレットの保護の規則だけはサーバーの `secrets_allowed` に任せる）で読み直す。
サーバーの側はジョブの fetch の経路 `/api/v1/runner/jobs/{job_id}/repo.git`（`RepoRead` × `PinnedCopyCtx`、`refs/pipelines/<id>/head`
だけを広告、want は固定したコミットから、`refs/pipelines/` は人の Smart HTTP から隠す）。FG-CI-0007〜0010、runbook `ci-runner` の
「Runner のホストの準備」。**Runner の登録の Web の画面はまだ無い**: 運用者のコマンド `forge admin runner register|list|disable`
（`--admin` に有効な人の管理者を記録する）で代える。`gstn` のコマンドは足していない。

**C5 の注記（2026-11-11）**: `ci/pipeline` は worker のタスク `ci_outputs` が system（ラベル `forge-ci`、報告者なし）で
`checks::write_run` を通して書く（コミットの最新のパイプラインだけ。`ci_pipelines.check_status` で 1 状態 1 回）。`CheckWrite` は HTTP からの
`ci/pipeline` を誰にも断る（`ci_namespace_internal`、`ci` の割り当てを持つサービスアカウントも）。`ci/` の他の名前は切り替え（X）までサービス
アカウントのまま。Action `CiLogRead`・`PipelineDiagnosisRead`（ADR-0029 の `PipelineDiagnosisRead`）、`Resource::CiOutput`、
`authorize_ci_output`（パイプラインの印と先端の印、`Forge-Context-Audience: agent` の宣言も AI として判定）。ルート
`GET …/pipelines/{id}/jobs/{job_id}/log`・`…/diagnosis`、`gstn pipeline log` / `diagnosis`、MCP の `get_job_log` /
`get_pipeline_diagnosis`（サーバーが判定し、`gstn mcp` は常に宣言する）。表 `failure_fingerprints`・`ci_job_failures`（抜粋はマスク済み、
読み取り用の接続は読めない）、マイグレーション `20261206200000`、`check_runs_reporter` を `ci/pipeline` に広げた。FG-AUTH-0022 の文面に
CI の出力を足した。**分けたもの**: 90 日の保持の削除（C5b）、CI 変数の値のマスク（C6）、成果物（C6）。

**C7 の注記（2026-11-11）**: ブランチは `feat/git-merge-result-staleness`（`review` は許された area に無いので `git`）。判定のジョブ
（`merge_request.mergeability`）が衝突なしのとき、その木を `commit-tree`（親は target の先端、head）でマージ結果にし、
`refs/merge-requests/<iid>/merge` を向ける（衝突では消す）。表 `merge_result_refs`（マイグレーション `20261207100000`、派生、(MR, head,
target の先端) ごとに 1 行）。MR のパイプラインは行を新しく書いたときに **マージ結果の SHA** で積む（`ci-pipelines` 購読者は `merge_request.pushed`
で head のパイプラインを作らなくなった）。必須チェックは今のマージ結果への実行だけを数え、前のマージ結果だけにあるものは
`required_check_stale`（最初の理由なら **FG-CI-0013** RequiredCheckStale、info、runbook なし）。外部の報告者の名前は従来どおり head への実行を
数え、`ci/pipeline` は head への実行を数えない。状態の契約に `merge_result_sha`・`checks[].stale` を足した（schema_version 1 のまま）。
隠すこと・push の拒否は既存の `refs/merge-requests/` の設定で足り、テストで固定した。新しいルート・Action は無い。詳細は ADR-0024 Amendment
（2026-11-11）。

**C5s の注記（2026-11-11、X の前）**: GitStation 自身の CI（`.forge-ci.yml` を `forge-runner` で動かす）を必須チェック `ci/pipeline`
にするには、試験が PostgreSQL に届く必要がある（`FORGE_TEST_DATABASE_URL`、`crates/forge-db/src/test_support.rs`。GitHub の
`.github/workflows/ci.yml` は postgres のサービスを使う）。ジョブのコンテナは `--network=none`（ADR-0029、`forge-runner/src/podman.rs`）なので、
今のままではどの DB にも届かない。計画 14 にはサービスが無かったので、X の前にこの段を足した。
ジョブの `services` は `{ name, image, env?, user?, tmpfs? }` の 2 つまで（名前は `[a-z][a-z0-9-]{0,30}`、ジョブの中で一意。イメージは
ジョブと同じくダイジェストで固定し `image_registries` の中。`env` は平文でシークレットではない。`tmpfs` は読み取り専用のルートの上の書ける場所で、
これが無いと PostgreSQL のようにデータとソケットのディレクトリに書くイメージが動かない。権限は足さない）。lint の理由は `too_many_services`・`service_name_invalid`・
`service_duplicate`・`service_env_invalid`・`too_many_service_env`・`service_user_invalid`・`service_tmpfs_invalid`・`too_many_service_tmpfs`
と既存の `field_missing`・`image_not_pinned`・`key_unknown`・`field_invalid`。`gstn ci plan` は各ジョブのサービスの名前を出す。
Runner はポッド `forge-job-<job>-<attempt>`（`--network=none`、Podman ではポッドとコンテナの名前は別なのでジョブのコンテナと同じ名前）を作り、
サービスのコンテナ `…-svc-<名前>` を切り離して起動してから、ジョブのコンテナをポッドに入れる。準備は待たない（スクリプトで
`until pg_isready -h 127.0.0.1; do sleep 1; done`）。起動の失敗は **FG-CI-0014** ServiceStartFailed（`reason=pod_create|service_start`、`service`、
`step`）。どの終わり方でも `podman pod rm --force --ignore --time=0`、起動時は残ったポッドも消す。ジョブが失敗するとサービスのログの最後の 50 行を
`[service <名前>]` 付きでジョブのログに足す（同じマスク）。本物の Podman（試験用の Runner のコンテナの中）で、ポッドの中の PostgreSQL 16 に
ジョブが 127.0.0.1 で届き、DNS も外への接続も無く、ポッドもコンテナも残らないことを確かめた。詳細は ADR-0029 の追記（2026-11-11）。

**C6a の注記（2026-11-12、シークレット）**: 表 `ci_variables`（マイグレーション `20261211100000`、プロジェクトに監査
`ci_variable.created` / `.updated` / `.deleted`、値は `ci.secret_keys_file` の鍵で封じる。読み取り用の接続は暗号文を読めない）、Action
`CiVariableManage`（Maintainer 以上、人だけ）、ルート `GET|PUT|DELETE /api/v1/projects/{id}/ci/variables[/{name}]`、`gstn ci variable
list|set|delete`（値は標準入力から）。Runner の取得は `secrets_allowed()` のときだけ値を `secrets` で渡し、渡せないジョブは Runner に渡さずに
失敗にする（FG-CI-0016 鍵・0017 許されないパイプライン・0018 登録の無い名前）。Runner は名前を定義と照合し、`--env-file`（ジョブの
ディレクトリの外、0600、create の直後に消す）で渡し、値と符号化をログでマスクする（チャンクの境目にまたがっても）。サーバーも保存の前に
マスクする。doctor `ci.secret_keys`、runbook `ci-secrets`、ADR-0029 の追記。`protected_only` は今は範囲を広げない（義務 5 が優先）。

## 6. 運用で足すもの

- **`forge doctor`**: `ci.runners`（有効な Runner の数・最後の連絡）、`ci.secret_key`（鍵のファイルの有無・権限）、`deploy.known_hosts`（ターゲットごとの固定の指紋）。
- **メトリクス**: 待ちのジョブの数と最古の待ち時間、リースの喪失、ジョブの時間、デプロイの結果、キューの長さ。
- **ログ**: すべての失敗に `request_id`・`error_code`、CI は `pipeline_id`・`job_id`・`runner_id`、デプロイは `deployment_id`・`target`。シークレットとジョブのトークンは出さない。
- **runbook**: `ci-runner`、`ci-secrets`、`deploy`（新規）、`checks-reporting`（改訂）。

## 7. 危険と対策

| 危険 | 対策 |
|---|---|
| ジョブのスクリプトが機密の内容を外へ出す | 出力は印を継ぐ（ADR-0029）、外への通信は既定で閉じる、ジョブのトークンは push もチェックの報告もできない |
| Runner のホストの侵害 | Runner のトークンはリポジトリを読めない。Job のトークンは 1 ジョブ・1 SHA・終わりで失効 |
| リースの取りこぼしでジョブが止まる | 心拍とリース、reaper、CI-e の alert |
| 自動化が prod のデプロイを承認する | DeployApprove の prod は人の Web のセッションだけ（権限の表の deny の行） |
| `ci/` の名前を他の主体が書く | `ci/pipeline` は HTTP から予約、切り替えの後は `ci/` 全体を system だけに |

## 8. 完了の条件（R1 の CI の判定）

1. forge の `main` の必須チェックが `ci/pipeline` になり、マージ結果で判定され、古いチェックはマージを止める。
2. 社内アプリ 1 本が stg へ自動でデプロイされ、prod へ人の承認でデプロイされる。stg の自動ロールバックが動く。
3. 先端に機密の文書のあるリポジトリで CI が通り、AI はそのログ・成果物を FG-AUTH-0022 で断られる。
4. 新しいルートと Action はすべて権限の表にある。カタログ・runbook・ADR-0029〜0031 がそろう。
5. 評価役 2 つの eval が緑。

**確かめのコマンド**: 計画 11 §9 と同じ（fmt・clippy `-D warnings`・nextest・`gen-catalog` / `gen-authz` の差分なし・`node scripts/self-heal.js --check`）。

## 9. 推奨で決めたこと（2026-11-10）

| # | 項目 | 決定 |
|---|---|---|
| 1 | チェックの名前 | `ci/pipeline`（任意で `ci/<job>`） |
| 2 | シークレットの鍵 | ADR-0013 の鍵のファイル |
| 3 | イメージ | 社内のレジストリのミラーだけ、ダイジェストで固定、外への通信は既定で閉じる |
| 4 | 鮮度 | 保護ブランチでは古い必須チェックはマージを止める |
| 5 | キュー | バッチは 4 まで、失敗で二分 |
| 6 | 保持 | ログ 90 日、成果物 30 日（デプロイした版は残す） |
| 7 | SSH の git 転送 | M4 の終盤 |
| 8 | prod の承認者 | Maintainer 以上、MR の作成者を除く（2 人の承認は任意、既定は無効） |

## 10. 利用者に聞くこと（事実が要る）

1. **Runner のホスト**: 推奨は試験サーバーとは別の Linux ホスト 1 台（rootless Podman）。用意できるホストと台数（C4 の前）。
2. **最初に自動デプロイするアプリ**とそのサーバー、stg があるか（C11 の前）。
3. **社内の GitLab に既存の Runner があるか**（移行と並行運用の計画のため。X の前）。
