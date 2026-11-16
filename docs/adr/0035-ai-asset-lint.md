# ADR-0035: AI 資産の lint と `forge/risk` の 2 つの規則、プロジェクトの設定 `ai_assets.enabled`

**Date**: 2026-11-15
**Status**: proposed（accepted は P17-3 `feat/core-ai-asset-lint` の `rust-reviewer`・`security-reviewer` のレビューで CRITICAL / HIGH が無いとき）
**Deciders**: Masaki Fujikawa（ユーザー、2026-11-13「この計画で進めて」: 計画 17 §0.4・§1.7・§2・§4・§9）、Claude

番号について: 計画 17 は P17-3 の判断を「ADR-0033 で決める」と書いたが、0033 は SSH（C13）、0034 は `forge/ai-eval`（P17-2）が使った。
`main` と開いている枝に 0035 以降が無いことを確かめて 0035 にした。

## Context

計画 17 は PFS の中央 AI の資産（`pe-*.md` のスキル、`me_ai_playbook` のプレイブック、エージェントの許可リスト、モデル、評価ケース）を
GitStation のリポジトリ `pfs-ai/assets` で直す。PFS の適合ゲート（分冊 36 §6）は「プレイブック ↔ スキル ↔ 許可リスト ↔ プロファイル」を
起動の前に確かめるが、今はそれを **MR の段で** 確かめる道具が無い。またケースは実際の外れの **合成の写し** だけを入れる（§4）。
生のデータ（絶対の時刻、実在の工場コード、IP、人、フィードバックの自由記述）がリポジトリに入ると戻せない（流出）。

同時に計画 17 §9 は「`ai_assets.enabled = true` にしたプロジェクトの外の動きを何も変えない」と決めた。普通のリポジトリにも `evals/` や
`skills/` という名前のディレクトリはある。

## Decision

1. **lint は forge-doc の中の独立したモジュール `forge_doc::ai_asset`（文書の型 doctype にはしない）**。
   - doctype（ADR-0026）は「前付けが `doctype` を名乗る `.md` の文書」と、その型の定義（`.forge/doctypes/*.yml`）の形で、見出し・ブロック・
     参照を検査する。AI 資産は TOML・JSON・前付けつきの Markdown が混ざり、形は PFS が決めたもの（`pe-*.md` のキー、`me_ai_playbook` の列）で、
     検査の大半は **ファイルをまたぐ整合**（プレイブックが名指すスキル・許可リスト・プロファイル）。doctype に載せると型の定義の言葉を広げる
     必要があり、`forge/doc-lint` と `forge/spec-*` の答え（計画 17 §9: 変わらない）に触れる。
   - 新しいクレートにしない: 依存の向き（CLAUDE.md の契約 9）を変えるにはエージェントモデルの側の変更が先に要る。forge-doc は純粋で、
     `forge-core`（`forge/risk` の規則）と `gstn`（`forge-client`）の両方から使える唯一の場所。
   - forge-doc の依存に純粋な解析の `toml`・`serde_json` を足した（`tests/purity.rs` の許可の一覧も）。どちらも既にツリーにある。
2. **形（v1）**: `skills/<name>.md`（前付けのキーは PFS の 11 個だけ、`name` = ファイル名、`required_profile` ∈ full / forms / core /
   forwarder、`axis: process` は `process_kind` が要る）、`playbooks/*.toml`（`[[playbook]]` 1 つ = `me_ai_playbook` の 1 行）、
   `agents/<name>.toml`（`name`・`allowed_tools`・`required_profile`。PFS-8 までは無くてよく、無ければエージェントの検査はしない）、
   `tools/tools.toml`（`[[tool]]` の `name`・`required_profile`。無ければ道具の検査はしない）、`models/models.toml`（帯ごとに
   `name:tag@sha256:<64 桁>`。`latest`・タグ無し・ダイジェスト無しは拒む）、`bench/bench.toml`（`*image` のキーはダイジェスト固定）、
   `evals/cases/<anomaly_class>/<id>.json`（`id` = ファイル名、`anomaly_class` = ディレクトリ（`security/` は除く）、`kind` ∈ regular /
   safety / reproduction（`security/` は safety）、`provenance` ∈ replica / synthetic、replica は判断 ID のハッシュ `source_hash`、
   `classification` は internal / public だけ、`expected` が要る）、`thresholds/` は `thresholds/proposals/*.md` だけ。
   - プロファイルは単調（forwarder < core < forms / full。forms と full はどちらも core より上で、互いには上下が無い）。プレイブックの
     `required_profile`（無ければ forwarder）は、名指すスキル・道具・エージェントのプロファイル以上でなければならない（`profile_too_low`）。
   - 規則の名前は安定（`skill_unknown`・`tool_not_allowed`・`model_latest`・`case_raw_data` など）。一覧は errors.toml の FG-AIASSET-0001。
3. **ケースの生のデータの検査 `case_raw_data`** は lint と `forge/risk` が **同じ関数** を使う（`forge_doc::ai_asset::case_raw_data`）。
   文字の上で探す: `YYYY-MM-DD`（時刻つきも）、IPv4、`P` + 3 桁で `P9xx` 以外、人のキー（`staff_id` など）、自由記述のキー（`feedback`
   など）、一度だけのキー（`provenance` など）の 2 回目。JSON として読み、`provenance` が replica / synthetic でない・`classification` が
   confidential も当たる。JSON の object でなければ「安全を示せない」で当たる（`not_json`）。見つけた値は決して写さない（種類と行だけ）。
4. **`forge/risk` の 2 つの規則**（`forge_core::risk`）:
   - `ai_case_raw_data`（`stop`）: 追加・変更された `evals/cases/**.json` の中身に生のデータ。読めなかったケース（上限を越えた・大きすぎる）
     も同じ規則で止める（安全を示せない）。C9 の自動マージは **既存の 3 つ目の分類「流出（leak）」** として止める
     （`auto_merge::rule_class`。新しい stop の分類は足さない）。
   - `ai_asset`（`warn`）: 資産のディレクトリ（`skills/`・`playbooks/`・`agents/`・`tools/`・`models/`・`bench/`・`evals/`・
     `thresholds/`）と `CODEOWNERS` の変更（消す・外へ移すも）。理由の文は「AI 資産の変更: …」で始まり、MR のチェックの要約・注記と
     Kanflow に流れる要約にそのまま出る。自動マージは警告として進め、MR の自動マージの 1 行に「AI 資産の変更を含むので警告付きで…」と出す
     （`queued_with_warning` の `detail = ai_asset`）。`NOT_ASSESSED_RULES` には入れない（止めない）。
   - どちらも **`ai_assets.enabled = true` のプロジェクトだけ**: `forge-checks` の `Risk` は実行ごとに設定を 1 回読み（5 秒の時間切れ）、
     オフなら規則の関数を **呼ばない**（`ai_asset_path_findings` / `ai_case_content_kind`）。常に動く `path_findings` / `content_kind` は
     AI 資産の規則を決して出さない（試験 `the_always_on_rules_never_name_the_ai_asset_rules`、`rules_silent_when_ai_assets_disabled`）。
5. **プロジェクトの設定 `ai_assets.enabled`**: 表 `forge.project_ai_assets`（追記のマイグレーション `20261222100000`、`derived`、
   行が無い = オフ）。`GET /api/v1/projects/{id}/ai-assets`（`ProjectRead`）、`PUT …`（新しい操作 `AiAssetsSettingManage`）。
   - `AiAssetsSettingManage` は **Web のセッションだけ**（`is_session_only`: 人のトークンもエージェントのトークンも `session_required`）、
     **人だけ**（`is_people_only`: ボット — `pfs-ai-bot` を含む — とサービスアカウントはセッションでも拒む）、Maintainer 以上。
     自動マージの主体・CI のジョブ・代理（act-as）は生成された deny の行で拒む。権限の表に allow / deny の行
     （`matrix_rows_ai_assets.rs`・`matrix_rows_people.rs`）。
   - 変更は同じトランザクションで監査（`ai_assets.enabled` / `ai_assets.disabled`、資源はプロジェクト、詳細は `project_id`・`from`・`to`）。
     ロックしたプロジェクトで判定をやり直す（`reconfirm`）。
   - オフに戻して止まるのは lint・2 つの規則・（P17-7 の）リリースの作成だけ。この表の行のほかは何も消さない（履歴・過去のリリース・
     署名・デプロイの記録は残る）。
   - 画面: プロジェクトの設定に「AI 資産」の節（ja / en / id）。ボタンは `viewer.can_change` のときだけ。
6. **`gstn ai-asset lint`**（ローカルだけ、サーバーに何も送らない）: 上へたどって見つけたリポジトリ（または `--dir`）の資産のディレクトリと
   `CODEOWNERS` を読み（1 万ファイル・128 MiB まで、1 ファイル 256 KiB + 1 バイト、シンボリックリンクはたどらない）、問題があれば
   終了コード 1 と FG-AIASSET-0001。オプトインのリポジトリの CI のジョブが走らせ、必須の `ci/pipeline` の中に入る（計画 17 §2）。
   サーバー自身は lint を走らせない（`forge/risk` の生のデータの検査だけ）。
7. **リリースに `thresholds/` を入れない**: 純粋な関数 `release_problems`（`thresholds/` と `manifest/` を拒む）を用意した。
   使うのは P17-7 のリリースの作成。
8. **エラーコード**: FG-AIASSET-0001 `AiAssetInvalid`（info）。0005 は計画 17 §5.4 のモデルのダイジェストの不一致のために空けておく。

## Alternatives Considered

### doctype にする（`.forge/doctypes/ai-skill.yml` など）
- **Pros**: 既存の `forge/doc-lint` と `gstn doc lint` に乗る。
- **Cons**: TOML・JSON の資産を表せない。ファイルをまたぐ整合（許可リスト・プロファイル）は型の規則の言葉に無い。`forge/doc-lint` の答えを
  全プロジェクトで変えうる（計画 17 §9 に反する）。
- **Why not**: 形が合わず、オプトインの外に効いてしまう。

### リポジトリの中の印のファイル（`.gitstation/ai-assets.toml`）でオプトインする
- **Why not**: 計画 17 §0.4 のとおり。`.gitstation/**` は既に `agent_config` の stop で、MR 1 本で規則を外せてしまう。

### 新しい stop の分類「AI 資産」を足す
- **Why not**: 利用者の方針（自動マージが止まるのは 4 つの分類だけ）。生のデータは流出の分類の中の規則で足り、「人が必ず見る」は保護パスと
  CODEOWNERS（計画 17 §1.7）。

### 設定を Owner だけにする（`merge.auto` と同じ）
- **Why not**: 計画 17 §0.4 が Maintainer 以上と決めた（PFS の approver = `pfs-ai` の Maintainer）。

## Consequences

### Positive
- 普通のリポジトリは何も変わらない（設定の既定はオフ、常に動く規則は AI 資産の規則を出さない）。
- `gstn` と `forge/risk` が同じ関数で同じケースを判定する。見つけた値は要約・注記・ログのどこにも写らない。
- 自動マージは生のデータを流出として止め、AI 資産の変更は警告として通す（`main` へのマージは本番を変えない: 計画 17 §1.7）。

### Negative
- TOML の問題は行番号を持たない（行 0、`subject` に表とキー）。`toml` の値から位置を取らないため。
- 生のデータの検査は文字の形で探すので、合成の値が形の上で当たることがある（例: `P1xx` の形の識別子）。そのときは写しの書き出し器の側で
  仮名の形を変える（ケースを手で直さない）。
- 「ケースの削除と `expected` の変更は保護の変更」（計画 17 §2）は差分が要るので、この lint では扱わない（保護パスと CODEOWNERS で守る）。

### Risks
- PFS の実物の `pe-*.md` が v1 の形と違う → P17-4 で 7 本を移すとき lint を緑にする（計画 17 §7.1）。形を広げるときはこの ADR の Amendment。
- 設定を読めない（DB）とき `forge/risk` は書かずに失敗し、ジョブの最後の試行で `error` を書く（既存の扱い）。オフとみなして通すことはしない。
