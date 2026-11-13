# 17 PFS の中央 AI の資産を GitStation で回す — フィードバックサイクルの設計と運用

- 作成: 2026-11-13（`planner` 相当。GitStation は `main` 9ce2a9af、PFS は読み取りだけで確かめた）
- 状態: **決定**（2026-11-13、利用者「この計画で進めて」）。§8 の 5 つの問いは既定どおり決定。PFS 側の項目（§7.2）は PFS の仕様の確定を待つ。
- 依頼: 「これを使った最も合理的なフィードバックサイクルの設計と運用を検討してください」。前提として、PFS の AI 資産は GitStation で運用すると利用者が決めた。

## 0. 目標（1 文）と前提の点検

**目標**: 工場の深掘り AI（PFS の Flow B）が外した判断を、1 件ずつ「再現する評価ケース」に変え、GitStation の MR・ローカル Ollama の評価ベンチ・署名付きリリース・段階展開を通して、**直ったことを数字で示してから**本番の資産レジストリに入れる。

### 0.1 PFS に既にあるもの（作り直さない）

| 項目 | 実体 | 出典 |
|---|---|---|
| 判断の記録（使った版を刻む） | `te_ai_judgment_log`（理論値の版・モデル・エージェント・スキルの版） | マスター仕様 §17.1、§16-4 |
| オペレーターの結果入力 | `te_ai_feedback_log`（的中 / 誤報 / 対応 / 効果）。`verdict` は「観測された実結果」に錨付け | §17.1、§17.4 |
| 改善要求の器と出口の切り分け | `te_ai_improve_request`（5 状態、`symptom_kind` → `outlet_kind`、`applied` には `applied_ref` 必須） | 分冊 36 §6.4 |
| 資産の版とハッシュ | `me_ai_asset_registry`（`content_hash`、`valid_from`、`plant_code` ごとの承認）。ハッシュ不一致の本文は注入しない | 分冊 36 §5.6、決定 2026-10-21 #9 |
| プレイブック | `me_ai_playbook`（`valid_from` で追記、上書きしない） | 分冊 36 §5.4 |
| スキルの実体 | `pe-*.md` 7 本（frontmatter に `anomaly_class`・`required_tools`・`required_profile`・`reviewer`・`version`） | `docs/iotpf-build-kit/skills/` |
| 守り | 6 体のエージェント、道具の許可リスト、読み取りだけ、数値は道具から引用、egress は fail-safe、モデルは `name:tag` 固定（`latest` 禁止） | §14、§18.2 |
| **素アラートは AI に抑止されない** | AI が黙ることはあっても検出を握りつぶさない | §19 |
| しきい値は自動で調整しない | `me_zeva_threshold`。改定は人（editor / approver）。**資産レジストリには入れない**（決定 #63） | 分冊 36 §6.3、§7-4 |

### 0.2 足りないもの（この計画で埋める）

1. **評価ベンチが無い**（モデル選定・スコアのしきい値 0.7 が仮のまま。決定 2026-10-21「領域 model は未決」）。
2. **実際の外れから評価ケースを作る道が無い**（§20.1 は「フィクスチャは合成だけ」と決めたが、作り方が無い）。
3. **変更の確認・承認・リリースの道具が無い**（今は「手順書を書く → レジストリに手で登録」。誰が何を承認したか、どのファイルが入ったかを機械で示せない）。
4. **段階展開と新旧の比較が無い**（§18.2 は「eval → カナリア → 本番」と書くだけ）。
5. **登録の手順が手作業**（レジストリの昇格の手順は未決）。

### 0.3 前提への意見

- **AI の誤りは安全の事故ではなく品質の事故**（素アラートは必ず届く）。だから 1 件ごとの緊急リリースは要らない。**週 1 回の束ねたリリース**で十分速く、承認の手間が最も少ない。
- **GitStation は「資産の正本・確認・評価の実行・署名・配布」を持ち、PFS は「判断・記録・フィードバック・ベンチの中身（推論のコード）」を持つ**。評価ベンチの採点のコードは PFS の `iotpf-ai-service` と同じものでなければ意味がない。GitStation は GPU の Runner と結果の判定・報告だけを作る。
- **しきい値・データの不足・道具の不足・新しい指標は AI 資産の変更ではない**。このサイクルでは扱いを分け（§1.3）、リポジトリには「提案」しか入れない。
- エッジの配布（M4E）は数千台向けで、AI ノードは数台。**配布は C11（承認付きの SSH デプロイ）を使い、M4E は使わない**。
- **GitStation 側の変更はすべて選んだリポジトリだけに効く（オプトイン）**。ほかのリポジトリの動きは何も変えない（§9）。

### 0.4 オプトインの印 — プロジェクトの設定 `ai_assets.enabled`

| 案 | 判断 |
|---|---|
| リポジトリの中の印のファイル `.gitstation/ai-assets.toml` | `.gitstation/**` を変える MR は既に `forge/risk` の `agent_config`（`stop`）に当たる（ADR-0017 Amendment 2026-11-12）。また MR 1 本で誰でも規則を外せてしまう |
| **プロジェクトの設定 `ai_assets.enabled`（採用）** | DB のプロジェクトの設定（既定 `false`）。変えられるのは `native_role` で Maintainer 以上の人だけ（エージェント・ボット・サービスアカウントは不可）、監査に残す。`true` にするのは `pfs-ai/assets` だけ |

`false` のプロジェクトでは、§1.7 の規則 `ai_case_raw_data`・`ai_asset`、P17-3 の lint、`forge/ai-eval` の判定は **呼ばれもしない**（判定の入口で設定を見て、何も出さない）。

**属性の規則**:

- `ai_assets.enabled` は **実質リポジトリの属性**（1 プロジェクト = 1 リポジトリ）。
- 新しいリポジトリの種類（kind）にしない。kind は作成のときに決まって変えられず、ADR-0023 でその意味の大半を取り下げたため。設定なら後から付け外しでき、監査に残る。
- 切り替えられるのは **`native_role` で Maintainer 以上の人の Web のセッションだけ**。監査の行を同じトランザクションで書く。
- **エージェント・ボット（`pfs-ai-bot` を含む）・サービスアカウント・自動マージの主体は決して切り替えられない**。権限の表に deny の行を置く（P17-3 の MR）。
- `false` に戻すと止まるのは lint・規則・`forge/ai-eval`・リリースの作成だけ。履歴・過去のリリース・署名・デプロイの記録は残り、PFS のローダーは既存のリリースを確かめ続けられる。

## 1. サイクルの全体

```
① 検出(ZEVA) → ② AI の判断(記録: 束の版・各ハッシュ・モデル name:tag+digest・プロンプトの版・入力ハッシュ・mode)
 → ③ 人のフィードバック(的中 / 誤報 / 見逃し + 理由の分類)
 → ④ 振り分け(どのレバーか。AI 資産でなければ Kanflow の PBI へ)
 → ⑤ 改善要求 → ⑥ 再現ケースを作る(PFS で合成の写しに変換) → ⑦ 下書き MR(最初のコミット = 赤いケース)
 → ⑧ 直す(人 / Claude Code / M5 のローカル LLM) → ⑨ 評価ベンチ(GPU Runner、全ケース、基準との比較)
 → ⑩ 確認と承認 → ⑪ マージ → ⑫ 週のリリース(タグ・マニフェスト・署名)
 → ⑬ 段階展開(影 → 1 工場のカナリア → 全体) → ⑭ レジストリに valid_from の行(ローダーが作る)
 → ⑮ 新版の監視・自動の戻し → ⑯ 閉じる(applied_ref = リリース + MR、旧版で判定した未解決の再評価)
```

### 1.1 判断の記録（②、PFS-1）

`te_ai_judgment_log` に次を足す（追記のマイグレーション）。**これが無いと③以降のすべてが「どの版の誤りか」を言えない。**

| 列 | 中身 |
|---|---|
| `asset_bundle_version` / `asset_bundle_digest` | 使った束（リリース）の版と署名した要約 |
| `asset_hashes` JSONB | 使ったエージェント・スキル・プレイブックの行ごとの `content_hash` / 行 ID |
| `model_small` / `model_medium` | `name:tag@sha256:<digest>`（タグだけでは中身が変わりうる） |
| `prompt_version` | エージェント定義の版（束の中のファイルの要約） |
| `input_hash` | 判定のスナップショット＋道具の応答を決まった並びにした SHA-256（再現と重複の検出） |
| `run_mode` | `live` / `shadow` |
| `sampling` | 温度・シード・`think` の有無 |

### 1.2 人のフィードバック（③）

L4 の画面で 3 択＋理由。理由は既存の `symptom_kind` をそのまま使う（新しい語彙を作らない）。

| 入力 | 意味 | 既定の理由 |
|---|---|---|
| 的中 | 要因が実結果と合う | — |
| 誤り | 要因が違う・数字の読みが違う | `wrong_focus` / `knowledge_gap` |
| 見逃し | 実際の要因を出さなかった・保留すべきでないのに保留 | `knowledge_gap` / `tool_gap` / `data_gap` |
| 余計 | 添付すべきでないのに添付（保留すべきだった） | `wrong_focus` / `budget_over` |

**閾値汚染の守り（§17.4）はそのまま**: 起票には別の入力者の裏付けが要る。`verdict` は実結果に錨付け。

### 1.3 振り分け（④）— どのレバーか

週 1 回、PFS の editor が `triaged` にする（人の判断。分冊 36 §6.4 の 3）。

| 出口 `outlet_kind` | AI 資産か | 行き先 |
|---|---|---|
| `skill`（知識が足りない） | はい | GitStation `pfs-ai/assets` の MR |
| `playbook`（見るところが違う・予算） | はい | 同上 |
| `agent`（プロンプト・出力の制約） | はい（§2 の段階 2 から） | 同上（保護パス） |
| `model`（モデルの力不足） | はい | 同上（保護パス、§6） |
| `threshold`（鳴りすぎ・鳴らない） | **いいえ** | PFS で人が改定。リポジトリには根拠の提案（`thresholds/proposals/`）だけを置いてよい |
| `tool`（道具が足りない） | **いいえ** | PFS のコード → Kanflow の PBI |
| `data`（エッジが測っていない） | **いいえ** | エッジ・PFE → Kanflow の PBI |
| `indicator`（新しい指標） | **いいえ** | ZEVA のコード → Kanflow の PBI |

AI 資産でない出口でも、**再現ケースは作る**（⑥）。道具やデータが入った後に同じケースが緑になることで「直った」を示せる（それまでは `expected_fail` の印で基準に入れる）。

### 1.4 改善要求から下書き MR へ（⑤〜⑦）

1. editor が `triaged` にすると、PFS の連携（`pfs-ai-bot`、§3）が **ケースの書き出し器**（PFS-3）を動かす。
2. 書き出し器は実際の判定から **合成の写し** を作る（§4）。写しが現行の束で同じ外れ方をする（赤い）ことを、PFS の手元で 1 回確かめる。
3. `pfs-ai-bot` が GitStation に **ブランチ `improve/<request_id>` と下書きの MR** を作る。最初のコミットはケースだけ。MR の本文に改善要求の ID・出口・症状・判断の ID（番号だけ）。
4. **ボットのトークンは下書きの MR しか作れない**。準備完了・承認・マージはできない。

### 1.5 直す（⑧）

| 書き手 | 使ってよいとき |
|---|---|
| PFS の editor（人） | いつでも。スキルの判断基準（数値）は人が書くのが基本 |
| Claude Code（開発の側） | ケースが合成の写しで `internal` のとき（§4）。機密の印のファイルには届かない（ADR-0026） |
| M5 のローカル LLM（`forge-llm`） | M5 の後。下書きのコミットを別ブランチで提案するだけ（01 §6 L-5 と同じ） |

### 1.6 評価ベンチ（⑨）

**中身**: PFS の `iotpf-ai-service` のイメージ（ダイジェスト固定）に `bench` の入口（PFS-0）を足す。チェックアウトした資産を読み、ケースが持つ道具の応答を偽の Edge MCP で再生し、**本物のパイプライン**（計画の解決・フック・スキーマ・数値引用・採点・保留）を本物の Ollama で通す。

**置き場所**: GitStation の CI ジョブ。ジョブのコンテナは `--network=none`（ADR-0029）なので、**Ollama をジョブの `services` に入れる**（C5s のポッド。ループバックだけ共有）。GPU と読み取り専用のモデル置き場は、タグ `gpu-eval` の Runner のホストの設定が渡す（P17-1）。外への通信は増えない。

| 走らせ方 | いつ | 範囲 | 回数 |
|---|---|---|---|
| MR | push ごと | 変更に関わるケース（anomaly_class・プレイブック・スキルの参照から決定的に選ぶ）＋安全のケース全部 | 各 3 回 |
| 夜間（C12 の定期） | 毎晩、`main` | 全ケース | 各 3 回 |
| リリース候補・モデルの変更 | 週のリリースの前、モデルの MR | 全ケース、新旧の両方 | 各 5 回 |

**採点はすべて決定的**（§20.1）: 構造化出力の照合、要因の分類の一致、数値引用の一致、保留すべきときの保留。

| 指標 | 定義 |
|---|---|
| 要因の的中率 | 期待する要因の分類が上位の要因に入ったケースの割合 |
| 添付の適合率 | 添付した（スコアがしきい値以上）うち正しかった割合 |
| 誤報の率 | 保留すべきケースで添付した割合 |
| 保留の正しさ | 期待どおりに保留したケースの割合（**答えないことも合格**） |
| 安全のケース | 注入・egress・他工場・自己汚染（§20.1 の 4 つ）。**100 % が必須** |
| 時間 | 重大度ごとの p95（critical ≤ 60 秒） |
| 予算 | 道具の呼び出し数・トークン数の p95 |

**揺らぎの扱い**: 温度 0・シード固定・`think` 切り。それでも GPU では完全には一致しないので、**ケースごとに多数決**（3 回中 2 回で合格）。基準で 3/3 だったケースが 1/3 以下になったら回帰。基準で 1/3〜2/3 のケースは「不安定」の一覧に入れ、ゲートには数えず件数を報告する（不安定が増えること自体を週の指標にする）。

**ゲート（チェック `forge/ai-eval`）**:

1. 安全のケースがすべて合格。
2. この MR の再現ケースが合格（赤 → 緑）。
3. 基準で安定に合格していたケースの回帰が 0 件。
4. 合計の指標が基準より悪くならない（的中率 −2 pt、誤報 +2 pt、p95 時間 +20 % までは許す。数字は仮で、最初の 4 週の実測で決め直す）。
5. ベンチが動かなかった・GPU が無い・基準が無い → **`error`**（成功にしない。§5.4）。

ジョブはチェックを書けない（ADR-0029 #8）。ジョブは成果物 `eval-report.json` を残し、**forge の worker が読んで基準と比べ `forge/ai-eval` を書く**（`ci/pipeline` と同じ形）。
**`forge/ai-eval` を出すのは、`.forge-ci.yml` のジョブが成果物 `eval-report.json` を宣言しているパイプラインだけ**。宣言の無いパイプライン（ほぼすべてのリポジトリ）にはチェックの行そのものが現れず、必須のチェックにもならない。宣言があるのに成果物が無いときだけ `error`。

### 1.7 確認と承認（⑩⑪）

自動マージは既定の `full` のまま（利用者の方針）。理由: **`main` へのマージは本番を変えない**。本番を変えるのはリリースの承認（⑫⑬）で、そこに人が必ず入る。

| 変更 | マージ | 根拠 |
|---|---|---|
| `skills/`・`playbooks/`・`evals/cases/` の追加 | editor が「準備完了」にし、`forge/ai-eval` が緑なら自動マージ | 戻せる。本番はリリースで止まる |
| `skills/` の判断基準の数値の変更 | 上に加え、スキルの `reviewer`（製造の技術者）の承認 1（CODEOWNERS） | 技術の知識の正しさは評価だけでは測れない |
| `agents/`（プロンプト・許可リスト）・`models/`・`bench/`（ベンチのイメージの版・採点の設定）・`evals/baselines/`・ケースの削除や期待値の変更 | **保護パス**: approver（Maintainer）の人の承認が必須（ADR-0017 §2.7 の `native_maintainer_approval_required`） | ゲート自体を緩める変更・権限を広げる変更 |
| `thresholds/proposals/` | 人の承認。リリースに入らない（lint が拒む） | PFS の規則（しきい値は人） |
| 生データらしい内容を含むケース | `forge/risk` の `stop`（既存の 3 つ目の分類「機密の流出」に規則 `ai_case_raw_data` を足す） | 流出は戻せない |

**新しい `stop` の分類は足さない**（4 つのままにする）。「人が必ず見る」は保護パスと CODEOWNERS で足り、`forge/risk` の規則 `ai_asset` は `warn`（MR と Kanflow に「AI 資産の変更」と出すだけ）。
**`ai_case_raw_data` と `ai_asset` は `ai_assets.enabled = true` のプロジェクトだけで動く**（§0.4）。普通の開発・仕様のリポジトリにある `evals/` や `skills/` という名前のディレクトリには当たらない。

### 1.8 リリース（⑫）

- 週 1 回（既定: 水曜）、`main` からタグ `assets/vYYYY.WW.N` を切る。`hotfix` は同じ形で随時。
- サーバーがタグの SHA の木を読み、**マニフェスト**（ファイルごとの SHA-256、プレイブックの行、モデルの `name:tag@digest`、ベンチの結果の要約、元の MR と改善要求の一覧）を決まった並びで作り、**計画 16 A1 の Ed25519 で署名**する（同じ鍵・同じ要約の関数。種類 `pfs-ai-bundle`）。公開後は不変（A5 と同じ）。
- 署名はリリース候補のベンチ（新旧・各 5 回）が §1.6 のゲートを満たしたときだけ。

### 1.9 段階展開（⑬）と新旧の比較

C11 のデプロイの環境を 3 つ作る。ターゲットのスクリプトは AI ノードで PFS のローダー（PFS-5）を呼ぶだけ。

| 段 | 中身 | 承認 | 進む条件（既定） |
|---|---|---|---|
| `pfs-ai-shadow` | 新しい束を **影** で動かす。同じ判定を新旧で処理し、新は `run_mode=shadow` で記録だけ（L4 に出さない）。GPU が空いているときだけ、critical / warning だけ、1 時間の上限あり | approver（`DeployApprove`、人だけ） | 3 日 かつ 30 件以上。劣化・スキーマ不合格・予算超過が旧より増えない。新旧で要因が違った判定を approver が一覧で見た |
| `pfs-ai-canary` | 1 工場（`plant_code`）だけ新版を本番に | approver | 4 日。下の戻しの条件に当たらない。この間に来たフィードバックで旧より悪くない |
| `pfs-ai-prod` | 全工場 | approver | — |

**新旧の比較**: 影の期間は同じ `event_id` の対で比べる（要因が一致した割合、新だけ添付・旧だけ添付、時間）。カナリア以降はフィードバックが付いた判定を版ごとに集計する（的中率・誤報・保留）。

### 1.10 レジストリへの登録（⑭）

**人は行を手で書かない**。ローダーが署名を確かめ（GitStation の公開鍵を手元に固定。届かなくても手元の鍵で確かめる）、スキルの本文を `AI_SKILLS_DIR` に置き、L3 API で `me_ai_asset_registry`・`me_ai_playbook` に **変わった資産だけ新しい `valid_from` の行** を足す（`plant_code` はカナリアならその工場、全体なら全工場）。行の `note` に束の版と MR。ハッシュ不一致・署名不正なら何も変えず、旧版のまま動く。

### 1.11 監視と自動の戻し（⑮）

束の版ごとに PFS が出す指標: 劣化（`degraded`）の率、スキーマ不合格、予算超過、保留の率、数値引用の差し戻し、時間 p95、フィードバックの的中率。

**自動で戻す条件**（どれか 1 つ。数字は仮）: 劣化の率が旧版の 24 時間平均 +5 pt を 1 時間続ける / スキーマ不合格が 3 % を超える / critical の p95 が 60 秒を超える / 旧版が的中していた型で「誤り」が別の入力者から 2 件。

戻しは **直前に承認されたリリース** をローダーが再登録する（追記の行。削除しない）。**既に人が承認した版に戻るだけなので、承認済みの runbook の手順として自動で行う**（§8 の問い 2）。素アラートは影響を受けない。

### 1.12 閉じる（⑯）

- 全体に入ったら、ローダーが改善要求を `applied`（`applied_ref` = 束の版 + MR の URL）にする。
- 旧版で判定した未解決の記録を機械で抜き出し、再評価に入れる（§17.4 の最後の段）。
- 人の作業が要る出口（`tool`・`data`・`indicator`・`threshold`）は Kanflow の PBI に（ADR-0005 / 0025 の連携）。PBI が閉じたら、`expected_fail` のケースを外す MR を作る。
- 本番で外れた判断が **既にベンチにあるケースと同じ型** だったら「回帰の漏れ」として記録し、ケースを足す（評価役の見逃しを eval にするのと同じ規則）。

## 2. リポジトリの形（GitStation の `pfs-ai/assets`）

**リポジトリは 1 つ**（グループ `pfs-ai`）。ケースを合成の写しにすることで、機密のリポジトリを分けずに済む（§4）。

```
pfs-ai/assets/
  CODEOWNERS                  # 保護パスと reviewer
  .forge-ci.yml               # lint → bench(MR) / bench-full(夜間・リリース)
  bench/bench.toml            # iotpf-ai-service と Ollama のイメージのダイジェスト、回数、許す差
  models/models.toml          # small / medium の name:tag@digest
  agents/<name>.toml          # プロンプト・許可リスト・出力スキーマの参照（段階 2。今はコードの中）
  skills/pe-*.md              # 正本を PFS のリポジトリから移す（frontmatter はそのまま）
  playbooks/<scope>.toml      # me_ai_playbook の 1 行 = 1 表
  thresholds/proposals/*.md   # 提案だけ。リリースに入らない
  evals/cases/<anomaly_class>/<id>.json   # 合成の写し・手作りの合成
  evals/cases/security/*.json # 注入・egress・他工場・自己汚染
  evals/baselines/<models>.json          # 夜間の結果から人が承認したもの
  manifest/                   # 生成物（リリースのときサーバーが作る。手で書かない）
```

- 保護ブランチ: `main`（push は MR だけ、force push 禁止）。
- 必須チェック: `ci/pipeline`（lint: frontmatter・プレイブックの参照・許可リスト ↔ レジストリ ↔ プロファイルの三者の整合・ケースの形）、`forge/ai-eval`、`forge/risk`。
- 保護パス: `agents/**`・`models/**`・`bench/**`・`evals/baselines/**`・`CODEOWNERS`・`.forge-ci.yml`・`thresholds/**`。ケースの削除と `expected` の変更は lint が「保護の変更」として同じ blocker を出す。

## 3. 役割と権限

GitStation の権限の形（ADR-0010、`authorize()` の 1 か所、権限の表に allow / deny の行）に合わせる。PFS の Keycloak（realm `iotpf`）でのログインは PFS の仕様の確定後で、それまでは GitStation のアカウントを対応させる。

| 主体 | GitStation のロール | できること | できないこと |
|---|---|---|---|
| PFS の editor | `pfs-ai` の Developer | ブランチ・MR・準備完了・スキルとケースの編集 | 保護パスの承認、リリース・デプロイの承認 |
| PFS の approver | `pfs-ai` の Maintainer | 保護パスの承認、基準の承認、リリースの切り出し、`DeployApprove`（影・カナリア・全体） | 自分の MR の承認（自己承認の禁止） |
| スキルの reviewer（製造の技術者） | Reporter + CODEOWNERS の個人 | 自分の分野のスキルの承認（Web の画面だけで済む） | 編集・マージ |
| `pfs-ai-bot`（PFS の連携の **ボットのアカウント**。所属は `pfs-ai` グループだけ） | エージェントの相手。グループの外のプロジェクトは見えない（404） | `improve/*` のブランチ作成と push、**下書きの MR** の作成、MR へのコメント | 準備完了・承認・マージ・保護パス・リリース・デプロイ・ほかのリポジトリ |
| CI の Job（`gpu-eval`） | ADR-0029 の Job | 自分の SHA の複製、成果物の保存 | チェックの報告、push、ほかの全部 |
| forge の worker | system | `forge/ai-eval` の書き込み、署名 | — |
| PFS のローダー（AI ノード） | 読み取りだけのトークン | 承認されたリリースの取得、公開鍵の取得 | 書き込みすべて |
| Kanflow | 代理（ADR-0025） | PBI の連携、イベントの受信 | 資産の変更 |

**AI に決してさせないこと**: 自分の MR を準備完了・承認・マージする / 保護パス（許可リスト・モデル・ベンチ・基準）を変えて通す / リリースやデプロイを承認する / レジストリの行を直接書く / しきい値を変える / フィードバックの `verdict` を書く。

## 4. データの扱い

| 入れてよい | 入れない |
|---|---|
| 合成の写し（下）、手作りの合成ケース | 生のセンサー値・波形、実在の機番・ロット・人の名前・社員番号 |
| 異常の分類・プロファイル・道具の応答の **形**（段差か傾斜か、比、件数） | 実際の工場コード・IP・時刻 |
| 判断・改善要求の **ID** | フィードバックの自由記述（要約しても入れない。注入の経路になる） |

**合成の写し**（PFS-3 の書き出し器、決定的）: 工場は `P9xx`、機器・ロット・人は乱数の仮名、時刻は判定からの相対、数値は「外れの形」を保ったまま種付きの生成器で作り直す（例: 段差の比、連の長さ、卓越周波数 ÷ 回転周波数の比は保つ）。`provenance: replica`、元の判断 ID の **ハッシュ** だけを持つ。写しが同じ外れ方をしない（形が保てない）ときは GitStation に入れず、PFS の中で `localOnly` のケースとして持つ（PFS の評価は件数だけを報告）。

- 機密の印（ADR-0026）: 写しは `internal`。`classification: confidential` のファイルは lint が拒む（このリポジトリには置かない）。だから Claude Code のエージェントもケースを読んで直せる。
- 保持: ケースは消さない（回帰の資産）。外すときは保護の変更。CI の成果物は 30 日、リリースの成果物は残す（14 §9-6）。

## 5. 運用

### 5.1 周期（推奨: 改善要求ごとの MR + 週 1 回のリリース）

| 案 | 判断 |
|---|---|
| A 改善要求ごとにリリース | 承認と影の期間が重なって回らない。AI の誤りは安全の事故ではない |
| **B（採用）MR は 1 件ずつ、リリースは週 1 回** | MR は小さく原因が追える。承認は週 1 回。影 3 日＋カナリア 4 日で、外れから全体まで約 2〜3 週 |
| C 月 1 回 | 学習が遅い。ケースが溜まって 1 回の差分が読めない |

| 曜日 | 何をする | 誰 |
|---|---|---|
| 月 | 週の集計（OBSERVE）、振り分け（`triaged`） | editor |
| 火 | MR の確認・準備完了 | editor、reviewer |
| 水 | リリースの切り出し、影の承認 | approver |
| 翌月曜 | 影の結果を見てカナリア | approver |
| 翌金曜 | 全体 | approver |
| 毎晩 | 全ケースのベンチ、基準の候補 | CI |

### 5.2 サイクル自体の指標

| 指標 | 目標（仮） |
|---|---|
| 外れの記録から全体への反映まで | p50 ≤ 21 日、p90 ≤ 35 日 |
| フィードバックの付いた判断の割合 | ≥ 30 % |
| ケースの網羅（有効なプレイブックごとに、保留のケースを含め 3 件以上） | 8 週で 100 % |
| 回帰の漏れ（ベンチにある型の本番での外れ） | 0 件。出たら必ずケースを足す |
| 夜間のベンチの成功 | ≥ 95 % |
| 不安定なケースの数 | 増やさない |
| 自動の戻し | 月 1 回以下。超えたらゲートの数字を見直す |

### 5.3 runbook（新規）

`ai-eval-bench`（ベンチが動かない・GPU・基準）、`ai-asset-release`（切り出し・署名・展開・登録）、`ai-asset-rollback`（自動の戻しの確認と手動の戻し）、`ai-model-update`（§6）。PFS 側は `iotpf-ai-service` の runbook に「ローダーの失敗」「影の止め方」。

### 5.4 サイクル自体の壊れ方

エラーコードの領域 `AIASSET`（仮。登録の前にエージェントモデルの root で領域を足す）。

| 壊れ方 | 扱い | コード（仮） |
|---|---|---|
| GPU の Runner が無い・落ちた | `forge/ai-eval` = `error`。MR は待つ（人が承知して通すこともしない） | FG-AIASSET-0002 BenchUnavailable（warn、runbook `ai-eval-bench`） |
| GPU が混んでいる | Runner `gpu-eval` は同時 1、時間切れ。Kanflow・PFS の本番の Ollama とは GPU を分ける | FG-CI-既存（時間切れ） |
| 基準が無い・モデルと合わない | `error`。夜間の結果から基準の MR を作る | FG-AIASSET-0003 BaselineMissing（info） |
| 回帰 | `failure`、回帰したケースの一覧 | FG-AIASSET-0004 EvalRegression（info） |
| 再現ケースが現行で既に緑 | `failure`（再現になっていない） | FG-AIASSET-0006 CaseNotReproducing（info） |
| モデルのダイジェストが `models.toml` と違う | ベンチを止める | FG-AIASSET-0005 ModelDigestMismatch（**alert**） |
| マニフェスト・lint の不正 | `failure` | FG-AIASSET-0001 ManifestInvalid（info） |
| ケースに生データらしい内容 | `forge/risk` `stop` | FG-REVIEW-既存の分類に規則を足す |
| ローダーの署名検査の失敗 | PFS は旧版のまま。O1 に報告 | PFS の `AI-*`（PFS-5 で登録） |
| 自動の戻し | 記録と通知 | PFS の `AI-*`、GitStation のデプロイのイベント |

## 6. モデルの更新は普通の変更として扱う

- 新しいモデルは **MR 1 本**: `models/models.toml` の `name:tag@digest` を変える。`latest` とダイジェスト無しは lint が拒む。
- モデルのファイルは外から取らない（§14.4 の供給層）。管理者がオンプレのモデル置き場に入れ（runbook `ai-model-update`）、Runner も AI ノードも同じダイジェストを確かめる。
- MR では **新旧の両方で全ケースを各 5 回**。比較表（指標ごとの差、回帰・改善したケース、時間、VRAM）を MR に出す。保護パスなので approver の承認が要る。
- 基準も同じ MR で新しいモデルのものを足す（`evals/baselines/<models>.json`）。
- Ollama 本体の版もベンチのイメージのダイジェストで固定し、変えるときはモデルの更新と同じ扱い（出力が変わりうるため）。
- 展開は通常の束と同じ 3 段。モデルの変更は影を 7 日に延ばす。
- これで PFS の未決（モデルの選定と評価ベンチ、スコアのしきい値 0.7）を **このベンチの結果で決める道** ができる。最初の MR は qwen3:14b を基準にする。

## 7. 作るもの

各 MR の終わりは計画 14 §5 と同じ（fmt・clippy・nextest、`gen-catalog` / `gen-authz` の差分なし、`/review` で CRITICAL / HIGH なし、評価役は 2 つまで）。

### 7.1 GitStation

| # | ブランチ | 中身 | 先に書くテスト（例） | 評価役 | 着手 |
|---|---|---|---|---|---|
| P17-0 [root] | `chore/harness-p17-ai-asset-area` | 領域 `AIASSET`、`ci-runner-security-reviewer` に GPU とモデル置き場の観点と eval | eval `gpu-service-gets-network`、`model-store-writable` | `harness-optimizer` | 今すぐ |
| P17-1 | `feat/ci-runner-gpu-model-store` | Runner のホストの設定で、タグ `gpu-eval` の Runner だけが **サービスのコンテナ** に GPU（CDI）とモデル置き場（読み取りだけ、ダイジェストの確かめ）を渡す。`.forge-ci.yml` からは要求するだけで、パスは書けない。**タグ `gpu-eval` を要求したジョブだけがこの Runner に割り当てられ**、タグの無いジョブの割り当て・コンテナの設定は今と同じ | `untagged_job_never_assigned_to_gpu_runner`、`gpu_denied_on_untagged_runner`、`model_store_mounted_read_only`、`job_container_has_no_gpu`、`pod_still_network_none` | `ci-runner-security-reviewer`、`rust-reviewer` | P17-0 の後（C5s は済み） |
| P17-2 | `feat/ci-ai-eval-check` | worker が成果物 `eval-report.json`（スキーマ固定）を読み、基準と比べて `forge/ai-eval` を書く（揺らぎの多数決、回帰、許す差、`error` の扱い）。MR に比較表。`forge/ai-eval` は HTTP から予約。`eval-report.json` を宣言したパイプラインだけで動く。ADR-0033 | `no_ai_eval_check_without_declared_report`、`regression_when_stable_case_drops`、`flaky_case_not_counted`、`missing_report_is_error_not_success`、`job_cannot_write_ai_eval` | `rust-reviewer`、`silent-failure-hunter` | P17-0 の後 |
| P17-3 | `feat/core-ai-asset-lint` | 資産の lint（スキルの frontmatter、プレイブック ↔ スキル ↔ 許可リスト ↔ プロファイル、ケースの形と `provenance`、`latest` の拒否、リリースに `thresholds/` を入れない）を純粋な層に（forge-doc の doctype にするかは ADR-0033 で決める）。`gstn ai-asset lint`。`forge/risk` に `ai_case_raw_data`（`stop`）と `ai_asset`（`warn`）。プロジェクトの設定 `ai_assets.enabled`（既定 `false`、Maintainer 以上の人だけ、監査）を足し、lint と 2 つの規則は `true` のときだけ動く | `rules_silent_when_ai_assets_disabled`（`evals/`・`skills/` を持つ普通のリポジトリで何も出ない）、`ai_assets_setting_denied_for_bot`、`ai_assets_setting_denied_for_agent_service_account_and_auto_merge`、`ai_assets_setting_denied_for_token_session`、`disable_keeps_past_releases`、`case_with_absolute_timestamp_stops`、`model_latest_rejected`、`playbook_unknown_skill_rejected`、`confidential_case_rejected` | `rust-reviewer`、`security-reviewer` | P17-0 の後 |
| P17-4 [pfs-ai/assets] | `chore/ops-pfs-ai-assets-bootstrap` | グループとリポジトリ、保護ブランチ・保護パス・CODEOWNERS、`.forge-ci.yml`、`pe-*.md` 7 本と合成の安全のケースを移す | lint が緑、安全のケースが現行で緑 | `security-reviewer` | P17-1〜3 と PFS-0 の後 |
| P17-5 | `feat/auth-pfs-ai-bot` | `pfs-ai-bot` の行（ボットのアカウント、`pfs-ai` グループの所属だけ、`improve/*` だけ、下書きだけ）。足りない deny の行 | `bot_denied_outside_pfs_ai_group`、`bot_cannot_mark_ready`、`bot_cannot_touch_protected_path`、`bot_branch_outside_improve_denied` | `authz-matrix-auditor`、`security-reviewer` | C9 の後 |
| P17-6 | `feat/ci-ai-eval-nightly` | C12 の定期で夜間の全ケース、基準の候補の MR（人が承認） | `nightly_opens_one_baseline_mr` | `rust-reviewer` | C12 の後 |
| P17-7 | `feat/deploy-pfs-ai-bundle` | 種類 `pfs-ai-bundle` のリリース（タグ → マニフェスト → A1 の署名）、C11 の環境 3 つ、直前の承認済みへの戻し、`gstn ai-asset release` | `bundle_signed_only_when_bench_green`、`moved_tag_has_no_effect`、`prod_approve_denied_for_bot`、`rollback_targets_last_approved` | `deploy-safety-reviewer`、`security-reviewer` | A1・A5・C11 の後 |
| P17-8 | `docs/ops-pfs-ai-runbooks` | §5.3 の runbook、doctor `ai_eval.runner`・`ai_eval.baseline_age` | — | `error-contract-auditor` | P17-2 と同時 |
| P17-9 | `feat/ui-ai-eval-report` | MR の画面に比較表（製造の技術者が読める言葉）、影の新旧の差の一覧 | 読み上げ、色だけに頼らない | `a11y-architect`、`design-reviewer` | P17-2 の後 |
| P17-10 | `feat/cross-llm-ai-asset-draft` | M5 の `forge-llm` が改善要求とケースから修正の下書きを別ブランチに提案 | `draft_never_marks_ready`、`llm_down_leaves_request_open` | `security-reviewer` | M5 の後 |

M4E は使わない。計画 16 の A10（教訓の注入）とは別の道（こちらは製品の AI 資産、A10 は開発者の `.claude/`）。

### 7.2 PFS（**PFS の仕様の確定を待つ**。PFS-0 だけは未決「評価ベンチ」の答えなので先に始めてよい）

PFS の仕様の変更は **PFS のリポジトリで別の段として行う**（この計画では書き換えない）。対象の節: マスター仕様 Part V §16-4（再現性と版の刻印）・§17.1（`te_ai_judgment_log` の列、PFS-1）・§17.4（戻り経路とケースの書き出し、PFS-3）・§18.2（モデルの昇格、§6）・§20（メタループと評価ベンチ、PFS-0）、分冊 36 §5.5〜5.6（スキルの正本の移転と資産レジストリの登録、PFS-5・8）・§6.4（改善要求と GitStation の MR、`applied_ref`、PFS-4）、`apps/iotpf-ai-service/references/decisions.md`（領域 model、PFS-0・6）。

| # | 中身 | 依存 |
|---|---|---|
| PFS-0 | `iotpf-ai-service` に `bench` の入口（偽の Edge MCP でケースの応答を再生、本物の Ollama、`eval-report.json`）。今の合成カタログ（`P900`）と同じ形 | なし（先行してよい） |
| PFS-1 | `te_ai_judgment_log` の列（§1.1）。追記のマイグレーション | 仕様の確定 |
| PFS-2 | L4 のフィードバックの 3 択＋理由（`symptom_kind`） | 仕様の確定 |
| PFS-3 | ケースの書き出し器（合成の写し、決定的、赤の確認） | PFS-0・1 |
| PFS-4 | 改善要求 → GitStation API（ブランチと下書きの MR）、`applied_ref` の書き戻し | PFS-3、P17-5 |
| PFS-5 | ローダー（署名の確かめ、スキルの置き場、`valid_from` の行、工場の範囲、戻し）。`AI-*` のコード | P17-7 |
| PFS-6 | 影のモード（`run_mode=shadow`、GPU の空き・重大度・上限） | PFS-1 |
| PFS-7 | 版ごとの指標と戻しの信号（O1 と C11 の健全性の確認へ） | PFS-5・6 |
| PFS-8 | エージェント定義をコードから `agents/*.toml` に出す（段階 2） | PFS-5 |

### 7.3 順序

```
P17-0 → (P17-1 ‖ P17-2 ‖ P17-3 ‖ PFS-0) → P17-4 → P17-8      … 段階 1: MR ごとのベンチが動く
      → C9 → P17-5 → PFS-1〜4                                … 段階 2: 外れから下書き MR まで自動
      → C12 → P17-6 ; A1・A5・C11 → P17-7 → PFS-5〜7          … 段階 3: 署名・展開・登録・戻し
      → P17-9 ; M5 → P17-10 ; PFS-8                         … 段階 4
```

段階 1 だけでも、今の手作業の登録の前に「全ケースで回帰が無い」を示せる。最初の成果はモデルの選定（§6 の最後）。

## 8. 危険と利用者に聞くこと

| 危険 | 対策 |
|---|---|
| ゲームのように基準を下げる・ケースを消す | 基準とケースの削除は保護パス、approver の人の承認 |
| ケースから工場のデータが漏れる | 合成の写しだけ、`ai_case_raw_data` の `stop`、写せないものは PFS の中 |
| ケースやスキルに注入の文 | フィードバックの自由記述を入れない、安全のケース 100 %、スキルは CODEOWNERS |
| ベンチと本番の差（偽の Edge MCP） | ベンチは本番と同じイメージ、影の期間で実データと比べる |
| 揺らぎで MR が赤くなり続ける | 多数決、不安定の一覧、許す差を 4 週で決め直す |
| GPU の取り合い | Runner `gpu-eval` を分け、同時 1。本番の AI ノードではベンチを動かさない |

**決定（2026-11-13、利用者の決定「この計画で進めて」。5 つとも既定どおり）**

1. 評価用の GPU: この開発サーバーの GPU を `gpu-eval` の Runner にしてよいか。**決定: はい**（Kanflow の Ollama とは時間で分け、夜間に全ケース）。
2. 自動の戻し: 本番（全体）でも、直前に承認された版へは人の承認なしに戻してよいか（CLAUDE.md 契約 7 の「承認済みの runbook の手順」として）。**決定: はい**。
3. スキルの正本: `pe-*.md` を GitStation に移し、PFS のリポジトリ（社内 GitLab）からは参照だけにするか。**決定: 移す**（GitLab への鏡は不要）。
4. スキルの数値の変更に製造の技術者の承認を必須にするか。**決定: 必須**（frontmatter の `reviewer`）。
5. 周期: 週 1 回のリリース（水曜）。**決定: はい**。

## 9. 既存の機能への影響

**この計画は、`ai_assets.enabled = true` にしたプロジェクト（`pfs-ai/assets`）の外の動きを何も変えない。**

| 既存の機能 | 変わるか | 理由 |
|---|---|---|
| システム開発の MR・承認・CODEOWNERS・保護パス | 変わらない | 既存の仕組みを `pfs-ai/assets` の設定として使うだけ |
| CI（`.forge-ci.yml`、`ci/pipeline`、Runner の割り当て、ジョブのコンテナ） | 変わらない | `gpu-eval` は要求したジョブだけ。`forge/ai-eval` は `eval-report.json` を宣言したパイプラインだけ |
| 自動マージ（既定 `full`）と `stop` の分類 | 変わらない。**`stop` の分類は 4 つのまま** | `ai_case_raw_data` は 3 つ目（流出）の中の規則で、オプトインのプロジェクトだけ。`ai_asset` は `warn` |
| デプロイ（C11、prod は人の承認、凍結、戻し） | 変わらない | 環境 3 つを `pfs-ai/assets` に足すだけ |
| 仕様・文書のリポジトリ（doctype、`forge/doc-lint`、`forge/spec-lint`、`forge/spec-trace`、`forge/spec-drift`） | 変わらない | AI 資産の lint は別の入口で、オプトインのときだけ呼ぶ |
| 既存の API・`gstn` のコマンド | 変わらない | 足すのは `gstn ai-asset …` と新しいルートだけ（どれも権限の表に行） |
| 権限の表の既存の行 | 変わらない | 行を足すだけ（`pfs-ai-bot`、設定の変更、リリース） |

**ほかのプロジェクトでも使える汎用の部品**（PFS 専用にしない）:

| 部品 | 使い道 |
|---|---|
| GPU の Runner のタグとモデル置き場（P17-1） | ほかの社内アプリの LLM・ML の評価や推論の試験 |
| 評価のチェックの報告者（P17-2、`eval-report.json` の形と基準との比較） | プロンプトやモデルを持つどのリポジトリでも、宣言すれば使える |
| 署名付きリリースの種類（P17-7、A1 の鍵と要約） | 種類を足せば、設定やモデルの束を署名して配る用途に使える |
| グループに閉じたボットのアカウント（P17-5、下書きの MR だけ） | 外のシステムから改善の下書きを出す連携の型 |
