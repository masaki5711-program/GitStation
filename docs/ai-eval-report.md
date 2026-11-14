# AI の評価の報告 `eval-report.json`（v1）と `forge/ai-eval`

計画 17 P17-2、ADR-0034。AI の資産（プロンプト・スキル・プレイブック・モデル）を持つリポジトリが、評価ベンチの結果を GitStation に渡すための **固定の形**。
型は `forge_core::ai_eval`（`EvalReport`、検査は `parse`、比較は `evaluate`）。この文書と型がずれたら型が正しい（この文書を直す）。

## 使い方（宣言したパイプラインだけ）

1. `.forge-ci.yml` のベンチのジョブが、作業ツリーの **ルート** に `eval-report.json` を書き、成果物として宣言する:

   ```yaml
   bench:
     stage: eval
     image: registry.internal/iotpf-ai-service@sha256:<64 hex>
     timeout: 2h
     tags: [gpu-eval]
     script: [bench --out eval-report.json]
     artifacts:
       paths: [eval-report.json]
     rules:
       merge_requests: true
   ```

   宣言はパスが **ちょうど `eval-report.json`** のときだけ（`out/eval-report.json` などは宣言にならない）。選ばれたジョブの 2 つ以上が宣言すると、
   パイプラインは `invalid`（FG-CI-0001 `eval_report_declared_twice`）。
2. リポジトリに基準 `evals/baselines/<models>.json`（同じ v1 の形。夜間の結果から人が承認したもの）を置く。`<models>` は報告の `models`。
   基準は **ベースのブランチ（MR のターゲット、無ければ既定のブランチ）との merge base** から読む。MR の中で基準を変えても、その MR 自身の判定には
   効かない。基準の変更は先に別の MR で入れる。`evals/baselines/**` は保護パスにする（承認者の人の承認が要る。計画 17 §2）。
3. forge の worker（タスク `ci_ai_eval`）がパイプラインの状態を追い、コミットのチェック `forge/ai-eval` を書く。
   **宣言の無いパイプラインには、このチェックの行は現れない**（必須にもならない）。

ジョブはチェックを書けない（ADR-0029 決定 8）。`forge/ai-eval` は `forge/` の名前なので、HTTP からは誰も書けない（管理者・サービスアカウントも）。

## 形（v1）

```json
{
  "schema": "gitstation.ai-eval-report/v1",
  "models": "qwen3-14b",
  "cases": [
    { "id": "spindle/replica-0001", "kind": "regular", "runs": 3, "passes": 3 },
    { "id": "security/injection-01", "kind": "safety", "runs": 3, "passes": 3 },
    { "id": "bearing/improve-0042", "kind": "reproduction", "runs": 3, "passes": 2 }
  ],
  "metrics": {
    "cause_hit_rate": 0.82,
    "attach_precision": 0.91,
    "false_alarm_rate": 0.04,
    "abstain_accuracy": 0.88,
    "p95_latency_ms": 41000
  },
  "allowed": { "cause_hit_rate_drop": 0.02, "false_alarm_rate_rise": 0.02, "p95_latency_rise_ratio": 0.2 }
}
```

| フィールド | 規則 |
|---|---|
| `schema` | `gitstation.ai-eval-report/v1` だけ。ほかは `schema_unsupported` |
| `models` | `[a-z0-9][a-z0-9._-]{0,62}`、`..` を含まない。基準のファイル名になる |
| `cases` | 1〜5000 件（空は `cases_empty`）。`id` は `[a-z0-9][a-z0-9._/-]`（128 バイトまで、空・`.`・`..` の区切りなし）で重複なし。`kind` は `regular` / `safety` / `reproduction`。`runs` は 1〜15、`passes` は `runs` 以下 |
| `metrics` | 率は 0〜1、`p95_latency_ms` は 0 以上。5 つとも必須 |
| `allowed` | 省略可。**基準のものだけを使う**（報告の `allowed` は無視。ジョブが自分のゲートを緩められない） |

知らないフィールドは拒む。全体で 1 MiB まで。形が違えば `forge/ai-eval` は `error`（FG-AIASSET-0007、`reason` と `detail` はフィールドやケースの番号で、本文は出さない）。
アップロードのときに測った sha256 と保存されたファイルが合わないときも同じ（`stored_digest_mismatch`）。

## 判定（計画 17 §1.6）

- **ケースごとの多数決**: `passes * 2 > runs` で合格。全部の回が同じ（全部合格か全部不合格）なら **安定**、そうでなければ **不安定**。
- ゲート（どれかに当たれば `failure`）:
  0. 基準にあるケースは **基準の `kind`** で判定する。報告が別の種類を書けば `case_kind_changed`（`regular` → `reproduction` だけは認める）
  1. 基準にある安全のケースが報告にあり、基準の回数以上動き、**全部の回で合格**（`safety_case_missing` / `safety_runs_short` / `safety_case_failed`）
  2. 再現のケースが多数決で合格（`reproduction_not_fixed`）、かつ基準で既に全部の回で合格していない（`case_not_reproducing`、FG-AIASSET-0006）
  3. 基準で全部の回で合格していた `regular` のケースが報告に無い（`case_missing`）、または今は半分以下（`stable_case_regressed`）。**基準で不安定なケースは数えない**（一覧に出し、件数を報告）
  4. 指標が基準より許す差を越えて悪くない: 的中率 −0.02、誤報の率 +0.02、p95 の時間 +20 %（基準の `allowed` で変えられる）。`attach_precision`・`abstain_accuracy` は表に出すだけ
- `error`（成功にしない）: 宣言したジョブが成功していない・報告が無い・ファイルが消えた（FG-AIASSET-0002）、報告が v1 でない（0007）、基準が無い・読めない・`models` が違う（0003）。
- 読めない失敗（保存先・git・リポジトリの準備中）は数えて、10 秒から最大 10 分まで間をあけてやり直す。5 回目（内部の誤りはすぐ）に `error`
  （FG-AIASSET-0002 `read_failed`）。24 時間を越えて待ち・実行中のパイプラインも `error`（`pipeline_stale`）。
- 合計の指標はベンチの数字をそのまま使う（ベンチのイメージはダイジェスト固定）。

## MR に出るもの

チェックの要約（Markdown）: 判定、指標の表（基準・今・許す差）、所見の一覧（コードと理由）、ケースの表（止めるもの → 不安定 → 直ったもの → 不合格 → 合格の順、最大 200 行。残りは件数）。
製造の技術者向けの言葉の画面は P17-9。

## まだ入っていないもの

- `ai_assets.enabled`（計画 17 §0.4）は P17-3 が足す。P17-2 の入口は「パイプラインが報告を宣言したか」だけ（ADR-0034 の決定 1）。
- 重大度ごとの p95、予算（道具の呼び出し・トークン）の指標は v1 に無い（v2 で足すときは `schema` を上げる）。
- マージキューの列車は `forge/ai-eval` を待たない（ADR-0034 の結果）。
