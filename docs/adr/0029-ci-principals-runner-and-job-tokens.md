# ADR-0029: CI の主体 — Runner と Job の資格情報、CI の信頼の境界

**Date**: 2026-11-10
**Status**: proposed（計画 14 §3.2 の決定を ADR にした。accepted は C0 `feat/auth-ci-principals` の `security-reviewer`・`authz-matrix-auditor` のレビューで CRITICAL / HIGH が無いとき）
**Deciders**: Masaki Fujikawa（ユーザー、2026-11-10「推奨で進めて」）、Claude

## Context

M4 の CI（計画 14）では、別のホストの `forge-runner` がジョブを取りに来て、ジョブのコンテナが対象のリポジトリを fetch し、成果物を出す。今の主体は人・個人のトークン・サービスアカウント・代理（`Credential::Session` / `Token` / `ActAs`）だけで、どれも CI に合わない。

- サービスアカウントのトークンは長命でプロジェクトをまたぎ、漏れたときの範囲が広い。1 つのジョブ・1 つの SHA に絞れない。
- AI の複製の拒否（ADR-0026 の 2026-11-10 の決定、FG-AUTH-0022）は `Principal::is_agent_audience()` が真の主体に、既定のブランチの先端に機密の文書があるリポジトリの clone・fetch を断る（`authorize.rs` の `RepoRead` × `RepoCopy` の arm）。CI のジョブを今の主体（サービスアカウント）で表すと、**機密の文書のあるリポジトリで CI が動かない**。
- 逆に CI に機密の内容を読ませると、ログ・成果物・診断（計画 04 P-9）を通って AI に渡る道ができる。AI が書いた `.forge-ci.yml` のスクリプトは中身を出力に書ける。
- `is_agent_audience()` は文脈 API の FG-AUTH-0021（ADR-0023 Amendment X5）も使う。意味を変えると両方の境界が動く。

壊れたときに、どの主体がどの規則で通った・断られたかをログだけで言えることが要る。

## Decision

1. **新しい資格情報を 2 つ足す**（`forge-auth/src/model.rs` の `Credential`）。
   - `Credential::Runner(RunnerCredential)`: 管理者が登録した Runner。**Runner の API の Action だけ**（ジョブの取得の long-poll・心拍とリース・ログの追記・完了）。リポジトリ・API の読み取りは一切できない。トークンは ADR-0019 と同じ形（SHA-256 のハッシュだけを `ci_runners` に保存、平文は Runner のホストのファイル、ADR-0013）。
   - `Credential::Job(Box<JobCredential>)`: ジョブを Runner に割り当てたときに 1 つ発行する短命のトークン。**1 つのプロジェクト・固定した SHA・そのジョブの生存期間**に結びつく。期限は `ジョブの時間切れ + 猶予`、ジョブが終わる（成功・失敗・取り消し・リースの喪失）と同じトランザクションで `revoked_at` を書く。要求のたびに DB で失効を確かめる（署名だけの JWT にしない）。
   - どちらも人の `UserId` を持たず、`admin_rights()` は常に false。`Principal` は forge-web の認証だけが作る（`Principal::runner` / `Principal::job`。試験 `only_forge_web_builds_ci_principals`）。
2. **判定は新しい `job.rs` で、`authorize()` の 1 か所の中**。`authorize()` は `Credential::Runner | Job` を最初に `job::decide` へ渡す（`act_as::decide` と同じ形）。許可は `Action::ci_rule() -> CiRule { Denied, Runner, Job }` の 1 つの網羅的な `match` で、**既定で閉じる**。`Denied` の Action はすべて行列に `ci_denied_for_<action>` の行を生成する。
3. **ジョブの複製は自分のプロジェクトの固定した SHA だけ、機密の印に関係なく許す**。新しい `Resource::PinnedCopy { project, sha }`（`authorize_pinned_copy` だけが作る）で、`job.rs` は `project == credential.project` かつ `sha == credential.sha` かつジョブが生きていることだけを見る。`CopyMark` は見ない。Job が `RepoCopy` や `Project` で求めたら、他のプロジェクト・他の SHA と同じく FG-REPO-0005 で断る（`reason=job_requires_pinned_copy` はログだけ。C0 のレビューで、見えるプロジェクトだけ 403 になる存在の手がかりを閉じた）。Smart HTTP の経路はジョブに隠した ref `refs/pipelines/<pipeline_id>/head` だけを広告する（`transfer.hideRefs` で他を隠す）。届くのはその SHA とその祖先だけ。
4. **`is_agent_audience()` は変えない**。Job・Runner の主体は人ではないので真を返すが、`job.rs` が先に決めるので `RepoCopy` の arm にも文脈 API にも届かない（`ContextRead` などは `CiRule::Denied`）。
5. **出力は元のリポジトリの印を継ぐ**。ログ・成果物・診断・失敗の指紋を読む Action（`CiLogRead`・`CiArtifactRead`・`PipelineDiagnosisRead`）は `Resource::CiOutput { project, pipeline_mark, tip_mark }` で判定する。`pipeline_mark` はパイプラインを作るときにその SHA を測った値（`ci_pipelines.copy_mark`、測れなければ `Undecided`）、`tip_mark` は読むときの既定のブランチの先端。人は `RepoBrowse` と同じ。**エージェントの相手は両方が `Open` のときだけ**。それ以外は FG-AUTH-0022（`route` = `ci_log` / `ci_artifact` / `ci_diagnosis`、`reason` = 印の名前、`mark_of` = `pipeline` / `tip`）。
6. **サービスアカウントは今のまま**: CI の主体にしない。エージェントの相手として、機密のリポジトリの複製と CI の出力を断られる。M3 の `ci/gates` のスクリプトは切り替え（計画 14 X）までサービスアカウントで報告を続ける。
7. **チェック `ci/pipeline` は Job も Runner も書かない**。forge の worker が `system` の主体（ラベル `forge-ci`）で DB の経路から書く（`forge/` と同じ形）。`CheckWrite` は HTTP からの `ci/pipeline` を常に断り（`reason=ci_namespace_internal`）、切り替えの後はそのプロジェクトの `ci/` 全体を断る。
8. **Job でできないこと**: push、チェックの報告、MR の操作、シークレットの読み取りの API（シークレットは割り当てのときに Runner へ渡し、保護ブランチのパイプラインだけ。計画 14 C6）、他のパイプラインの成果物、文脈 API。

### Amendment: ADR-0026（2026-11-10、AI の複製の拒否）への追記

ADR-0026 の安全網（FG-AUTH-0022、`RepoRead` × `RepoCopy`）は CI のジョブには届かない。ジョブは決定 3 の `PinnedCopy` で `job.rs` が判定し、印を見ない。代わりに CI の出力が決定 5 で同じ拒否を受けるので、機密の内容が CI を通って AI に渡る道は閉じたまま。運用の規則（機密の文書はエージェントが読めないリポジトリに置く）は変わらない。FG-AUTH-0022 の `cause`・`user_message` は C5 で CI の出力を含む文に改める（`errors.toml` と生成物を同じコミット）。

## Alternatives Considered

### B: CI をサービスアカウントと個人のトークンで動かす（gitlab-runner の形）
- **Pros**: 新しい資格情報が要らない
- **Cons**: トークンが長命でプロジェクトをまたぐ。機密のリポジトリで CI が FG-AUTH-0022 で止まる。止めないために例外を足すと、同じサービスアカウントの API の読み取りも通る
- **Why not**: 漏れたときの範囲を 1 ジョブ・1 SHA に絞れない

### C1: `is_agent_audience()` から Job を外す
- **Pros**: 既存の `RepoCopy` の arm がそのまま通る
- **Cons**: 文脈 API（FG-AUTH-0021）の定義も動く。境界の意味が 2 か所で変わる
- **Why not**: 共有の定義を変えず、Job は専用の判定で決める

### C2: AI が起こしたパイプラインのジョブをエージェントとして扱う
- **Pros**: AI の push が機密の内容に届かない
- **Cons**: AI が push するたびに機密のリポジトリの CI が落ちる（AI 駆動の開発の主な経路が止まる）。漏れの道はジョブの読み取りではなく出力なので、出力を閉じれば足りる。「誰が起こしたか」の判定が自動マージ・再実行・キューで曖昧になる
- **Why not**: 決定 5 で出力を閉じる方が単純で、境界が 1 つ

### Job のトークンを署名だけの JWT にする
- **Pros**: DB を引かない
- **Cons**: ジョブの終わりに即座に失効できない
- **Why not**: 「終わりで失効」を守れない

## Consequences

### Positive
- 機密の文書のあるリポジトリでも CI が動き、AI には出力を渡さない
- 判定は `authorize()` の 1 か所のまま。CI の主体は既定で閉じ、行列の生成の行で全 Action を覆う
- Runner のホストが侵害されても、Runner のトークンではリポジトリを読めず、Job のトークンは 1 ジョブで切れる

### Negative
- `Credential` の変種が 5 つになり、`Principal` の構成子と `reconfirm` の引き継ぎが増える
- CI の出力を読むたびに既定のブランチの先端を測る（ADR-0026 のキャッシュを使う）
- 機密のリポジトリでは AI がパイプラインの診断で自分を直せない（人のアカウントで読む）

### Risks
- **ジョブのスクリプトが外へ送る** → 外への通信は既定で閉じる（計画 14 §9 の 3）。許すのは社内のミラーだけ。`ci-runner-security-reviewer` が定義の変更を見る
- **Runner のホストが全プロジェクトの中身を見る** → 機密のリポジトリ用にタグで Runner を分けられる形にする（C3）。Runner のホストでは AI のエージェントを動かさない
- **隠した ref の広告の漏れ** → `git-protocol-reviewer` が C0 で確かめる。試験 `job_fetch_sees_only_the_pipeline_ref`
- **印の測定の失敗を開いたとみなす** → `Undecided` は閉じる（ADR-0026 と同じ）

## 失敗の場合とログでの見え方

| 失敗 | コード | ログの項目 |
|---|---|---|
| Runner / Job のトークンが無い・期限切れ・失効 | FG-AUTH-0001 | `credential=runner\|job`、`reason=unknown\|expired\|revoked`、`job_id` |
| Job が他のプロジェクト・他の SHA、または固定した複製以外の `RepoRead` | FG-REPO-0005（見えないプロジェクトと同じ 404。見える・見えない・無いで同じ応答） | `reason=job_other_project\|job_sha_mismatch\|job_requires_pinned_copy`、`action`、`job_id` |
| Job の許されない Action | FG-AUTH-0003 | `reason=ci_not_allowed`、`action`、`ci_rule`、`job_id` |
| Runner が他の Runner のジョブ・終わったジョブ | FG-AUTH-0003 / CI-next（JobLeaseLost） | `reason=lease_other_runner\|job_finished`、`runner_id` |
| AI が機密の CI の出力を読む | FG-AUTH-0022 | `route`、`reason`、`mark_of`、`audience`、`pipeline_id` |
| HTTP から `ci/pipeline` を書く | FG-AUTH-0003 | `reason=ci_namespace_internal`、`action=CheckWrite` |

## 後の MR の義務（C0 のレビュー、2026-11-10）

C0 は型と判定だけを入れた（トークンの保存・発行・照会は無い）。次を後の MR が満たす。満たすまで ADR は accepted にしない。

**C3（Runner の API・トークン）**
1. プロジェクトの削除・移動・アーカイブと**同じトランザクションで**、そのプロジェクトのジョブのトークンに `revoked_at` を書く。
2. `JobTokenState::Expired` は **DB の `now()`** と `expires_at` を比べて決める（アプリのホストの時計を使わない）。`authorize()` は時計を持たないので、認証が読んだ状態を `JobCredential.state` に入れる。
3. `JobCredential.sha` は `forge_core::push::ObjectId` で、40 桁（SHA-1）か 64 桁（SHA-256）の**小文字の 16 進だけ**を受ける（`ObjectId::parse` が大文字・短縮・空白を拒む。C0 で確認済み）。`ci_pipelines.sha` も同じ形で保存し、比較は文字列の完全一致（短縮した SHA・ref 名で照合しない）。

C3 で満たした（2026-11-10）: 1 は `projects::delete_decided` と `namespaces::move_subtree`（プロジェクトとグループの移動）の同じトランザクションで
`revoke_project_job_tokens`（試験 `deleting_or_moving_the_project_revokes_its_job_tokens`。アーカイブは機能が無い）。2 は認証が DB の
`expires_at <= now()` を読む（`expiry_is_decided_by_the_database_clock`）。3 は `ci_job_tokens.sha` を `ci_pipelines.sha` から写し、`ObjectId::parse` で読む。
ジョブの終わり（完了・取り消し・リースの喪失）でも同じトランザクションで失効する。

**C5（CI の出力）**
4. ログ・成果物・診断のどの経路よりも先に、`Undecided`（測れなかった）の印でエージェントの相手の CI の出力の読み取りが断られる（閉じる）試験を入れる。

**C3 / C6 / C11 / C12（C2 のレビュー、2026-11-10 で追加）**
5. **シークレットを渡すかは作成時の記録だけで決める**: `ci_pipelines.source = 'push'` かつ `ci_pipelines.protected`（作成のときに保護されたブランチだったか、
   C2 で保存）のときだけ。`manual` / `merge_request` / `schedule` のパイプラインには **決して** 渡さない。実行の時点の保護の設定や ref 名から判定し直さない。
   C2 は `StoredPipeline::secrets_allowed()` とその試験（`only_a_protected_push_may_release_secrets`）を置いた。C3・C6 はこの関数だけで判定し、
   `secrets_allowed()` が偽のパイプラインのジョブにシークレットが渡らない試験を Runner の API の側にも入れる。
6. **保護された push のパイプラインの再実行**（`PipelineRetry`）は、シークレットを持つジョブもやり直す。Developer・エージェント・サービスアカウントに許すが、
   **デプロイのジョブ（C11）はそれとは別に人だけの `DeployApprove` を要る**（再実行が承認の代わりにならない）。C11 は「再実行されたデプロイのジョブが承認を待つ」試験を入れる。
7. 手動・定期の実行の保護（C12）: 今は常に保護されない扱い。C12 で変えるときも 5 の判定（作成時の記録）を通す。

## 追記（2026-11-10、C1）— `forge-ci` の置き場所と、シークレットの判定

- `forge-ci`（`.forge-ci.yml` の解析・lint・plan）は機能クレートではなく **純粋な層**（`forge-core → forge-ci → forge-doc → forge-errors`、契約 9、ADR-0020 の
  Amendment）。`gstn` と `forge-checks` が同じ lint を使える。パイプラインの作成と実行は後の機能クレート（仮称 `forge-pipelines`、C2）。
- シークレットは二重に止める。lint: シークレットを持つジョブは `merge_requests: false` で、ブランチのパターンがリポジトリの保護ブランチに覆われていること
  （`LintContext::with_protected_branches`。保護の一覧を知らない手元の検査 `LintContext::local_check` ではどのブランチも保護されていない扱い）。
  plan: `Trigger.protected` が偽（保護されていないブランチ）か MR のパイプラインで、選ばれたジョブがシークレットを持てば計画が失敗する
  （`secret_on_unprotected_ref`。黙って外さない）。C6 も保護ブランチにしか値を渡さない。

## Error codes and runbooks affected

- 既存のコードを使う: FG-AUTH-0001・0003・0022（0022 は文面の改訂、C5）。新しいコードは CI-next（JobLeaseLost、C3 で登録）。
- runbook: `ci-runner`（新規、C3）、`smart-http`（ジョブの fetch の節、C0）、`checks-reporting`（`ci_namespace_internal`、X）。
- `forge-auth/README.md` の「CI principals」の節（責務・公開 API）は C0 の MR で足す（act-as の節と同じ形）。
