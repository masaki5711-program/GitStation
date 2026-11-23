# Runbook: ai-asset-release（関連: FG-AIASSET-0001 `release_includes_thresholds` / `release_includes_manifest`、FG-AIASSET-0002〜0004、FG-DEPLOY-*）

計画 17 §1.8〜§1.10 の、`pfs-ai/assets` の週 1 回のリリース（切り出し → 署名 → 3 段の展開 → PFS のレジストリへの登録）の手順と、
途中で止まったときの見方。**本番を変えるのはこの手順の承認だけ**（`main` へのマージは本番を変えない）。

**今の状態（2026-11）**: 切り出しの前の確かめ（lint・夜間のベンチ）と C11 のデプロイは動く。署名付きの束（種類 `pfs-ai-bundle`、
`gstn ai-asset release`、3 つの環境）は **P17-7**、PFS のローダー（署名の確かめ・レジストリの行）は **PFS-5** で入る。それまでの段は
`（P17-7 以降）`・`（PFS-5 以降）` と書いた。入ったらこの runbook のコマンドとログのイベント名をその MR で埋める。

## Trigger

- 週のリリース（既定: 水曜。計画 17 §5.1）、または `hotfix`。
- リリースの途中で止まった: 署名されない（ベンチが緑でない）、展開の段が進まない・失敗した（FG-DEPLOY-*、runbook `deploy`）、
  ローダーが登録しない（PFS の `AI-*`）。

## Impact

- 止まったリリースは本番を変えない。旧版が動き続ける（AI の誤りは品質の事故。通常 SEV4）。
- 影（`pfs-ai-shadow`）は GPU を使う。PFS の本番の判定が遅れたら影を止める（PFS の `iotpf-ai-service` の runbook「影の止め方」）。

## 手順

### 1. 切り出しの前の確かめ（読み取りのみ）

1. `main` の最新の夜間のベンチ（C12 の定期のパイプライン）の `forge/ai-eval` が `success`。`error` なら runbook `ai-eval-bench`。
2. `main` で `gstn ai-asset lint` が何も出さない。リリースに `thresholds/`（提案だけ）と `manifest/`（サーバーが作る）を入れない
   （リリースの一覧の検査 `release_problems`: FG-AIASSET-0001 `release_includes_thresholds` / `release_includes_manifest`）。
3. 前のリリースから入った MR の一覧（`gstn mr list --project <id> --state merged`）と、関わる改善要求を確かめる。
   保護パス（`agents/`・`models/`・`bench/`・`evals/baselines/`）の変更があれば approver の承認が残っているか。

### 2. 切り出し `[MUTATING — needs approval]`（approver）

1. `main` の確かめた SHA に注釈付きタグ `assets/vYYYY.WW.N`（`hotfix` も同じ形で N を進める）。`assets/*` は保護タグにする
   （`gstn protected-ref` の `refs/tags/assets/*`。動かせない・消せない）。動いたタグは効かない（P17-7 `moved_tag_has_no_effect`）。
2. （P17-7 以降）サーバーがタグの SHA の木からマニフェスト（ファイルごとの SHA-256、プレイブック、モデルの `name:tag@digest`、ベンチの要約、
   MR と改善要求）を作り、**リリース候補のベンチ（新旧・全ケース各 5 回）が §1.6 のゲートを満たしたときだけ** 計画 16 A1 の鍵で署名する
   （`bundle_signed_only_when_bench_green`）。署名されないときはベンチのパイプラインの `forge/ai-eval` を見る（runbook `ai-eval-bench`）。
   鍵の問題は runbook `agent-signing-key`。

### 3. 段階の展開 `[MUTATING — needs approval]`（approver の `DeployApprove`、人だけ）

| 段 | 承認 | 進む条件（既定、計画 17 §1.9） |
|---|---|---|
| `pfs-ai-shadow` | approver | 3 日 かつ 30 件以上。劣化・スキーマ不合格・予算超過が旧より増えない。新旧で要因が違った判定を approver が一覧で見た（モデルの変更は 7 日） |
| `pfs-ai-canary`（1 工場） | approver | 4 日。自動の戻しの条件に当たらない。フィードバックで旧より悪くない |
| `pfs-ai-prod`（全工場） | approver | — |

- 要求と承認は C11 のデプロイ（`POST /api/v1/projects/{id}/deployments`、`…/approve`。runbook `deploy`）。要求者・その SHA の背後の人は
  承認できない。ボット（`pfs-ai-bot`）・エージェント・サービスアカウントは承認できない（`prod_approve_denied_for_bot`、P17-7）。
- （P17-7 以降）3 つの環境とターゲットのスクリプト（AI ノードで PFS のローダーを呼ぶだけ）は P17-7 で作る。それまでは展開しない。

### 4. 登録（PFS-5 以降。人は行を手で書かない）

ローダーが署名を確かめ、スキルを `AI_SKILLS_DIR` に置き、変わった資産だけ `me_ai_asset_registry`・`me_ai_playbook` に新しい `valid_from` の行を足す
（`note` に束の版と MR）。ハッシュの不一致・署名の不正なら何も変えず旧版のまま（PFS の `AI-*`）。全体に入ったら改善要求が `applied` になる。

## Diagnose（読み取りのみ。止まったとき）

1. タグ: `git ls-remote <repo> 'refs/tags/assets/*'`、タグの SHA が `main` の確かめた SHA と同じか。
2. ベンチ: そのタグの SHA のパイプラインの `forge/ai-eval`（`gstn mr status` / コミットのチェック）と worker のログ
   `forge.ai_eval.check_written`（`error_code`・`reason`）。
3. デプロイ: `SELECT id, status, error_code, failed_target FROM forge.deployments WHERE project_id = <id> ORDER BY id DESC LIMIT 5;`
   と runbook `deploy` の Diagnose。
4. 登録: PFS のローダーのログ（`AI-*`）。GitStation の側では、ローダーの読み取りのトークンでリリースと公開鍵が取れるか（P17-7 以降）。

## Mitigate `[MUTATING — needs approval]`

- ベンチが緑でない: リリースしない。原因の MR を直す（または戻す）MR を通し、次の週に切り出す。ゲートを緩めて通さない
  （基準・許す差の変更は保護パスの MR で、リリースの手順の外）。
- 展開の段の失敗: runbook `deploy`。前の段に戻すときは runbook `ai-asset-rollback`。
- 誤ったタグ: 消さない・動かさない。次の N で正しい SHA に切り直し、誤ったタグの版は展開しない。

## Verify

- 全体（`pfs-ai-prod`）の `forge.deployments` が `succeeded`、PFS の版ごとの指標（計画 17 §1.11）が旧版より悪くない。
- 改善要求が `applied`（`applied_ref` = 束の版 + MR の URL）（PFS-5 以降）。

## 関連

- 計画 17 §1.8〜§1.12・§5.1・§5.3、ADR-0034・ADR-0035、ADR-0031（デプロイ）、ADR-0032（A1 の署名）
- runbook `ai-eval-bench`、`ai-asset-rollback`、`ai-model-update`、`deploy`、`agent-signing-key`
