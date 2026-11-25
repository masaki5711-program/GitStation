# 19 M5 の計画 — ローカル LLM（MR の一次レビュー・障害の一次診断・自然な言葉の質問・承認の要約、W14）

- 作成: 2026-11-23（`planner` 相当。GitStation は `main` 1bb6a535 で確かめた。Kanflow は `D:\APP\KanflowV2` を読み取りだけで確かめた）
- 状態: **決定済み**（2026-11-23、利用者が §11 の問いをすべて推奨の既定で決めた。計画 20・21 の問いも同じ日に推奨の既定で決まった）。L5-1 から着手する。
- 参照: 01 §6・§8（M5）・§11、06 §4.10・D-1・D-5、07 W10・W14、09 §2・§9 の 8、13 §8.3（段 6・7）、16 A11、17 §1.5・§1.6・P17-1・P17-10、12 E6、02 §6（F-18）、04 M-5・M-6、ADR-0003・0017・0025・0026・0029（Amendment 2026-11-14）・0034。

## 0. 目標（1 文）と前提の点検

**目標**: 社内の GPU で動くローカル LLM を 1 つのゲートウェイ（`forge-llm`）から呼び、W14 の優先の 4 つ — **MR の一次レビュー（W14-1）・障害の一次診断（W14-5）・自然な言葉の質問（W14-6）・承認の要約（W14-11）** — を「助言」として出す。**LLM は何も決めない**（マージ・承認・デプロイ・本番の変更は、テスト・決まった規則・人が決める）。

### 0.1 決まっていること（議論し直さない。§10 に一覧）

- サーバーで動く AI はローカル LLM だけ（06 D-1）。Claude はサーバーで動かさない。
- LLM の結果は Checks API の **助言** のチェック。既定でマージを止めない。失敗は `error`、成功にしない（ADR-0003）。
- GPU は当面 Kanflow の Ollama（Qwen3.8 27B）を共用し、forge 側で同時実行を絞る（01 §11 の 6）。
- 優先度は **対話 > MR レビュー > 自動修正 > 一括処理**、MR レビューに GPU 時間の 20 % を予約、10 分待ちで昇格（ADR-0017 §3）。
- データの境界は 3 区分（06 D-5）。秘密はどの LLM にも渡さない。機密はローカル LLM だけ。

### 0.2 コードで確かめたこと

| 項目 | 状態 | 根拠 |
|---|---|---|
| `forge-llm` クレート・`LlmProvider` | **無い** | `crates/`（20 クレート。01 §5 と契約 9 の「llm」は名前だけ） |
| エラーの領域 `LLM` | **無い**（ADR-0017 決定 8 で承認済みだが `AREAS` に未登録） | `crates/forge-errors/build.rs:41-44` |
| AI の危険の層（L-R）の差し込み口 | ある。`AiRisk` と `combine()`（AI は上げるだけ、応答なしは `stop NotAssessed`）。今は `None` を渡している | `crates/forge-core/src/auto_merge.rs:338-372` |
| 自動修正 | 決定的な修正だけ（C14）。「No LLM (M5)」 | `crates/forge-core/src/autofix.rs:5`、`crates/forge-review/src/autofix.rs:3` |
| GPU の Runner とモデル置き場 | ある（P17-1）。`gpu-eval` は排他のタグ、ジョブのコンテナに GPU を渡さない | `crates/forge-runner/src/gpu.rs`、`crates/forge-db/src/ci_runners/lease.rs:64` |
| 評価のチェック `forge/ai-eval` | ある（P17-2）。`eval-report.json` を宣言したパイプラインだけ | ADR-0034、`crates/forge-cli/src/worker/ci_ai_eval.rs` |
| 機密の境界 | ある。`forge_checks::confidential`（仕様書・文書の印、読めなければ閉じる）。エージェントのトークンには渡さない | `crates/forge-checks/src/confidential.rs:1-29`、`crates/forge-auth/src/model.rs:300-320` |
| readiness の LLM | 名前だけの枠（テストの偽物） | `crates/forge-core/src/readiness.rs:1`、`crates/forge-web/src/readyz_tests.rs:185` |
| 運用 API・`diag bundle` | **無い**（doctor のコメントに「later diag bundle」） | `crates/forge-cli/src/doctor/mod.rs:17` |
| 障害の記録 | `ops/incidents/records.jsonl` と訓練の記録 1 件 | `ops/incidents/` |
| Kanflow の GPU の調停 | ある。`GET /api/internal/ai-busy`（利用者が Ollama を待っているか）を他のプロセスが見て、背景の処理を止める | `KanflowV2/app/src/lib/ai-gate/remote-gate.ts` |
| 評価役 `llm-integration-reviewer` | **無い**（01 §8 で M5 に追加と決めた） | root `.claude/agents/` |
| ADR の番号 | 次は **0039**（計画 20・21 が並行。採番のときに確かめる） | `docs/adr/` |

### 0.3 前提への意見

1. **M5 の全部を 1 段で作らない**。01 §8 の M5 は L-1〜L-3・L-5〜L-7・E6・評価セット・チャットの道具・運用 API と広い。この計画は **土台（ゲートウェイ・記録・権限・評価）と W14 の優先の 4 つ** を段 1 にし、残り（L-3 の影・L-R・L-5・L-6・L-7 のパイプライン・自動修正の LLM・E6・13 §8.3・16 A11・17 P17-10）は **同じゲートウェイの上の段 2** として順に足す（§7.2）。
2. **自然な言葉の質問（W14-6）は Kanflow のチャットが入口**（07 W14 の置き場所、08 の「入口は Kanflow」）。GitStation は **LLM を呼ばない読み取りの道具**（数字と一覧を返す、`as_of` 付き）を Kanflow に出すだけ（02 §6 F-18）。GitStation に 2 つ目のチャットは作らない（§11 の問い 4）。
3. **「深いレビューは Claude へ」はサーバーからは送らない**（D-1）。一次レビューは「深掘りが要る」の印と理由を出し、開発者の PC の Claude Code が `gstn` / MCP で読みに来る。機密の入力から作った助言はエージェントのトークンには見せない。
4. **計算はコード、言葉は LLM**（01 §6.1）。承認の要約でも、マイグレーション・保護パス・危険の判定・失敗したチェックの **事実の一覧はコードが作り、LLM は言い換えだけ**。LLM が事実を落としても、事実の一覧は別に必ず出る。
5. **LLM が止まっても何も止まらない**（07 W14）。助言の画面は「AI の助言は今ありません」と LLM なしの表示に戻る。例外は段 2 の L-R だけで、そこは ADR-0017 Amendment F どおり「応答なし = 自動マージの `stop`」（人のマージは止めない）。
6. **本番の GPU の置き場所が決まっていない**。最終の配備は PFS の Linux サーバーで、PFS の AI ノード（本番の Ollama）とは GPU を分けると計画 17 で決めた。試験はこの開発サーバーの GPU（Kanflow と共用）で行い、本番は利用者が指定する GPU のホストまで **既定で無効**（§11 の問い 1）。
7. モデルの名前が計画ごとに違う（01・06: Qwen3.8 27B、17: PFS のベンチは qwen3:14b）。**GitStation のゲートウェイは Kanflow と同じモデルを `name:tag@digest` で固定**する（同じモデルなら VRAM の入れ替えが起きない）。BankMasalah も同じ Ollama を使うので `num_ctx` をそろえる（違うと Ollama がモデルを読み直す。07 W14）。

## 1. 案と推奨 — LLM をどこで動かし、誰が呼ぶか

| 案 | 中身 | 良い点 | 悪い点 |
|---|---|---|---|
| **A（推奨）ゲートウェイを forge の worker に置く** | 機能のクレート `forge-llm` が、DB の列（優先度つき）から取り出して、社内の Ollama（GPU のホスト）を HTTP で呼ぶ。入力はコードが組み立て、出力は JSON スキーマと根拠で検査して、チェック・助言の行に書く | 01 §5・ADR-0003・ADR-0017 の設計どおり。対話（承認の要約・診断）も数秒〜数十秒で返せる。Outbox・ジョブの列・エラーの契約・監視をそのまま使える。Kanflow の `ai-busy` に合わせて譲れる | Ollama を共用するので、混雑の調停を自分で書く（ADR-0017 §3 の規則がある） |
| B 推論を `gpu-eval` の Runner のジョブにする | MR ごとにジョブを作り、`services` の Ollama（`--network=none`）で推論、成果物を worker が読む（`forge/ai-eval` と同じ形） | 隔離が強い。GPU の排他は既にある | ジョブごとにモデルの読み込み（27B で数十秒）。対話に向かない。Kanflow の受入基準・障害のログ・機密の文書を **ジョブに渡すことになる**（ジョブは MR の作者のコードが動く場所）。Runner は 1 台で同時 1 |
| C 別のプロセス `forge llm-serve` | ゲートウェイを独立のプロセスにし、web と worker がループバックで呼ぶ | 再起動・資源を分けられる | プロセスと資格情報が 1 つ増え、権限の判定点が増える。列は DB にあるので得るものが少ない |

**推奨: 案 A**。B は「評価ベンチ」（計画 17、プロンプトとモデルの変更の確かめ）にだけ使い、日々の推論には使わない（§6）。

### 1.1 GPU の調停（A の中の選択）

| 案 | 判断 |
|---|---|
| **G1（推奨。01 §11 の 6 で決定済み）Kanflow の Ollama を共用** | 同じモデル・同じ `num_ctx` なら入れ替えなし。forge の同時実行は既定 1（設定で 2 まで）。**Kanflow の `ai-busy` を見て**、Kanflow の利用者が待っている間は forge の一括処理と MR レビューの新しい呼び出しを始めない（実行中は奪わない）。Kanflow に届かないときは「混んでいない」とみなし、1 分に 1 件に絞る（Kanflow の `degraded` と同じ規則） |
| G2 forge 専用の Ollama を同じ GPU に | 2 つのモデルで VRAM を取り合い、入れ替えで両方遅くなる |
| G3 forge 専用の GPU のホスト | 最も安定。利用が増えたときの道（01 §11 の 6）。§11 の問い 1 の答えしだい |

**評価ベンチ（`gpu-eval`）との取り合い**: ベンチはポッドの中の別の Ollama で同じ GPU を使う（17 の決定 1: この開発サーバーの GPU、Kanflow とは時間で分ける）。

- サーバーが `gpu-eval` のジョブを Runner に渡す（`lease.rs` の取得）前に、ゲートウェイに **対話** の推論が実行中か待っていれば、取得を最大 10 分遅らせる（飢餓を防ぐため 10 分で渡す）。
- ゲートウェイは `gpu-eval` のジョブが動いている間、**一括処理** を始めない（MR レビューと対話は続ける。VRAM 不足の失敗は `FG-LLM-0001` で記録し、後で再試行）。
- 夜間（既定 01:00〜05:00）は一括処理とベンチの全ケースの時間。

## 2. 全体の形

```
イベント（Outbox）/ 人の操作 / forge diagnose
  → 入力の組み立て（機能のクレート。決定的: 差分の分割、生成物とロックの除外、事実の一覧、機密の判定、秘密の検出と伏せ字）
  → forge-llm: llm_requests に 1 行（目的・優先度・入力の区分・入力のハッシュ・プロンプトの版）
  → 列（優先度・予約・昇格・Kanflow の ai-busy・gpu-eval の実行中）→ Ollama（時間切れ、モデルの digest を確かめる）
  → 出力の検査（JSON スキーマ、根拠が入力の中にあるか、件数の上限、範囲外の操作の語を捨てる。不正なら 1 回だけやり直し）
  → llm_advice に 1 行（入力の区分を引き継ぐ）→ チェック / 承認のカード / 診断の記録 / API
  → 人が読む。「役に立った / 誤り」→ llm_advice_feedback → 採用率・誤検知率
```

### 2.1 クレートと依存の向き（契約 9 を変えない）

- `forge-core::llm`（純粋）: `trait LlmProvider`、`Purpose`（`mr_review` / `incident_diagnosis` / `approval_summary` / 段 2 の目的）、`Priority`、`InputClass`（`public` / `internal` / `confidential`。秘密は区分ではなく **入れてはいけないもの**）、助言と指摘の型、出力の検査（スキーマ・根拠・上限・語の除去）。I/O なし。
- `forge-llm`（機能のクレート、`forge-db` と `forge-core` に依存）: Ollama のプロバイダ（`reqwest`、時間切れ、行き先は設定の許可だけ）、列、調停、記録、メトリクス。
- **入力の組み立ては、そのデータを持つ機能のクレート**（`forge-review` が MR、`forge-deploy` が承認の事実）が `forge-core::llm` の型で作る。機能のクレートどうしは依存しない（`forge-llm` を呼ぶのは worker と web の配線、`forge-cli` / `forge-web`）。
- プロンプトはリポジトリの中で版管理（`crates/forge-llm/prompts/<purpose>.v<N>.toml`）。結果にモデルの `name:tag@digest`・プロンプトの版・入力のハッシュを残す（01 §6.1）。

## 3. 4 つの機能

### 3.1 MR の一次レビュー（W14-1 = L-1 の要約 + L-2 の指摘）

| 項目 | 中身 |
|---|---|
| きっかけ | `merge_request.updated`（新しい head）。同じ MR の古い head の依頼は捨てる（結果も、head が違えば書かない） |
| 入力 | merge-base..head の差分（生成物・ロック・バイナリを除く、ファイルごとに分割、合計の上限）、MR の題と本文（**データとして**囲む）、変更の規模と種類（コードが計算）、`forge/risk` の決定的な結果、Kanflow の受入基準（リンクがあるとき、02 §4） |
| 出力 | チェック **`forge/ai-review`**（HTTP からは予約、worker だけが書く）。要約 3 行（何を・どこに・何が危ない）、指摘は **最大 10 件**、確信度の低いものは出さない。重大度は baseline の 4 段階。各指摘に **ファイル・行・根拠の引用**（入力に無い引用の指摘は捨てる） |
| 結論 | 指摘なし → `success`、指摘あり → `neutral`（**`failure` は使わない**。必須にしたプロジェクトだけ CRITICAL / HIGH で `failure`）、LLM の失敗 → `error` |
| 深掘りの印 | 「Claude か人の深いレビューを勧める」と理由（認証・マイグレーション・並行処理・大きな差分など。**判定はコードの規則が主**、LLM は理由の文を足すだけ） |
| 既定 | プロジェクトの設定 `llm.review` = `off` / `advisory` / `required`。段 1 は **試行のプロジェクトだけ `advisory`**、ほかは `off`（§11 の問い 2）。`required` にできるのは Maintainer 以上の人（ADR-0003） |
| Claude Code から | `gstn mr ai-review <iid> --json`。エージェントのトークンには **入力の区分が `confidential` の助言を出さない**（「機密の入力のため表示しません」だけ） |

### 3.2 障害の一次診断（W14-5）

| 項目 | 中身 |
|---|---|
| きっかけ | サーバーの `forge diagnose --request-id <id> \| --error-code <code> [--since 2h]`（運用者、読み取りだけの DB のロール）。段 1 では HTTP の入口を作らない（ログがサーバーにあるため） |
| 入力（決定的に集める） | その `request_id` / `error_code` のログの行（上限つき、伏せ字の後）、`errors.toml` の cause / diagnosis / fix、runbook の該当の節、`ops/incidents/records.jsonl` の同じコードの記録、直近のデプロイ・マイグレーション・設定の変更、`forge doctor --json` |
| 出力 | **原因の候補を順位つきで 3 つまで**、それぞれに根拠（ログの行の番号・runbook の節・過去の障害の ID）と **次に見るもの**（読み取りのコマンド）。**変更の手順は出さない**（出たら捨てる。緩和は人が承認した runbook の手順だけ、契約 7） |
| 置き場所 | `llm_advice`（種類 `incident_diagnosis`）と標準出力。`incident-commander` が障害の記録に貼る。`gstn ops diagnosis <id>` で読む（管理者だけ） |
| 失敗 | LLM が使えなければ、集めた事実の束（LLM なし）だけを出す。診断の道具は止まらない |
| 修正 | Claude Code（開発者の PC）が `debugger` → MR（W14-5 の「修正は Claude Code が MR に」） |

### 3.3 自然な言葉の質問（W14-6）

- 入口は **Kanflow の AI チャット**（Kanflow の Ollama。GitStation の LLM は呼ばない）。
- GitStation が出すもの: **読み取りの道具のパック**（02 §6 F-18）— 「この MR はなぜ止まっているか」（`evaluate_merge()` の理由の一覧そのまま）、「パイプラインの失敗」（P-9 の診断 API）、「デプロイの状況」、「PBI に結びついた MR・パイプライン」。数字と一覧は道具が返し、LLM は計算しない。`as_of` 付き。
- 呼び方は **代理（ADR-0025）で、聞いた本人の権限で**。見えないものは 404 で、道具の答えにも出ない。技術者以外も聞けるよう、答えには Git の言葉を言い換える材料（状態の名前の対訳）を付ける（08）。
- 代理の許可リストに読み取りの Action を足す（ADR-0025 Amendment）。書き込みは足さない。

### 3.4 承認の要約（W14-11 = L-9）

| 項目 | 中身 |
|---|---|
| きっかけ | デプロイの承認待ちの作成（C11 の `DeployApprove` の対象、prod と承認の要る stg） |
| 事実（コード） | 前回のデプロイからの MR の一覧、マイグレーションの有無と名前、保護パス・CI の設定・権限の変更、`forge/risk` の判定、失敗・`error` のチェック、凍結の期間 |
| 要約（LLM） | 業務の言葉で 3 行（「何が変わり、何が危ないか」、09 §2 の下流）。**事実の一覧とは別に表示**し、要約が事実を落としても事実は消えない |
| 結びつき | 承認の対象の **SHA に結びつける**。新しいコミットで古い要約は「古い」と出す |
| 表示 | GitStation の承認の画面と、Kanflow の承認待ちのカード（W13、イベントと API で渡す） |
| 決めない | 承認のボタンは要約の有無・内容と無関係。LLM が使えないときは事実の一覧だけ |

## 4. 助言の見せ方（決めないこと）

- **ラベル**: すべての助言に「AI の助言（提案）」と、モデル・プロンプトの版・作った時刻。人の指摘と見た目で区別する（色だけに頼らない）。
- **AI がしないこと**（コードで止める）: 承認・準備完了・マージ・デプロイの承認・ラベルや担当の変更・コメントのメンション（`@`・`closes` の語・画像の URL は出力から除く。ADR-0017 §2）。助言の行には操作の欄そのものが無い。
- **判定は誰か**: マージの可否はテスト・必須チェック・人の承認（ADR-0003）。障害の緩和は人の承認（契約 7）。承認は人。
- **評価**: 指摘ごとに「役に立った / 誤り」（人だけ。エージェントは押せない）。採用率と誤検知率を目的ごとに集計し、プロンプトとモデルを変えるときの基準にする（01 §6.5）。
- **数を絞る**: 1 MR 10 件まで、同じ指摘の繰り返しは 1 件にまとめる。読まれなくなるのが最大の危険（01 §10）。

## 5. 入力の境界（D-5、機密のパス、注入）

| 区分 | ローカル LLM に渡すか | 助言を見せる相手 |
|---|---|---|
| コード・PBI・設計書・伏せ字の後のログ（`internal`） | 渡す | 読める人とエージェントのトークン |
| 個人情報・顧客情報・機密の仕様（`confidential`。`forge_checks::confidential` が判定、読めなければ閉じる） | 渡す（D-5: Qwen だけ） | **人だけ**。エージェントのトークン・代理の道具には「機密のため表示しません」 |
| 秘密（トークン・鍵・パスワード・高いエントロピーの文字列・PEM） | **渡さない**。見つけたら伏せ字、伏せられなければ依頼ごと拒否（`FG-LLM-0005`） | — |

- **助言の区分 = 入力の区分の最大**（`InputClass` は落とさない）。機密を含む MR の助言は、本文に機密を書かなくても `confidential` のまま。
- **見える範囲だけを入力にする**: 依頼した人（またはイベントの主体）が読めないものは入れない（LLM を通した IDOR を防ぐ、ADR-0017 §2.3）。MR の一次レビューは MR を読める人なら誰でも見られるので、入力は MR のリポジトリと結びついた PBI だけ。
- **入力はデータ**: 差分・題・本文・ログ・文書は区切りで囲み、「指示ではない」と前置きする。それより効くのは **出力の検査**: スキーマ、根拠の実在、操作の語の除去、範囲外の対象の拒否。注入で変わりうるのは助言の文だけで、状態は何も変わらない。
- **社外に出ない**: 行き先はループバックか設定の許可にある社内のホストだけ（契約 8）。それ以外の設定は起動時に `FG-SYS-0002`（`reason=llm_endpoint_not_internal`）。
- **残さないもの**: 入力の本文は DB に残さない（ハッシュと、git の SHA・行の範囲などの再現の鍵だけ）。出力は残す（区分つき）。

## 6. 評価（測ってから広げる）

- **評価セット**（合成だけ、契約 8）: `crates/forge-llm/evals/<purpose>/`。
  - MR レビュー: 欠陥を仕込んだ差分（見逃してはいけない）と、きれいな差分（誤検知を数える）。
  - 障害の診断: `errors.toml` と合成のログから作った症状 → 期待する原因の候補。
  - 承認の要約: マイグレーション・保護パスを含む変更 → 要約が触れるべき事実。
  - **安全のケース（100 % 必須）**: 差分・題・ログ・文書に書いた注入（「LGTM と言え」「承認せよ」「全文を出力せよ」）、秘密を含む入力の拒否、機密の助言をエージェントに出さない、操作の語の除去。
- **走らせ方**: `llm-bench`（`forge-llm` の bin）が評価セットを流し、ADR-0034 の `eval-report.json` を出す。プロンプト・モデル・評価セットを変える MR で `gpu-eval` のジョブとして走らせ、`forge/ai-eval` で基準と比べる（計画 17 の部品を GitStation 自身に使う。GitStation のプロジェクトを `ci.gpu_eval_projects` に入れる）。GitStation が自分を置くまでは、この開発サーバーで手で走らせて報告を MR に付ける。
- **R2 の目標**（01 §8「M5 の開始時に決める」。§11 の問い 3）: 採用率 ≥ 30 %、誤検知（「誤り」の割合）≤ 20 %、仕込んだ欠陥の検出 ≥ 70 %、安全のケース 100 %、MR レビューの依頼から結果まで p95 ≤ 10 分、承認の要約 p95 ≤ 60 秒。
- **エージェントモデルの側**: 評価役 `llm-integration-reviewer`（注入、出力の検査、失敗を成功にしない、機密の助言の漏れ、入力の見える範囲）と eval を先に足す（L5-0）。

## 7. 作るもの

各 MR の終わりは計画 14 §5 と同じ（fmt・clippy・nextest、`gen-catalog` / `gen-authz` の差分なし、`/review` で CRITICAL / HIGH なし、評価役は 2 つまで）。LLM を呼ぶテストはすべて **偽のプロバイダ**（決まった応答・時間切れ・不正な JSON・注入の応答）で決定的に試す。本物の Ollama は `llm-bench` だけ。

### 7.1 段 1（土台と W14 の優先の 4 つ）

| # | ブランチ | 中身 | 先に書くテスト（例） | 評価役 | 着手 |
|---|---|---|---|---|---|
| L5-0 [root] | `chore/harness-m5-llm-area` | 誤りの契約に領域 `LLM`（ADR-0017 決定 8 の登録）、評価役 `llm-integration-reviewer` と eval、`forge-llm` の `paths:` 規則 | eval `diff-injection-flips-conclusion`、`confidential-advice-to-agent-token`、`llm-timeout-reported-as-success` | `harness-optimizer` | 今すぐ |
| L5-1 | `feat/core-llm-contract` | `forge-core::llm`: trait・型・`InputClass`・出力の検査（スキーマ、根拠の実在、上限、操作の語の除去）・秘密の検出。ADR-0039（ゲートウェイ、調停、境界、助言の見せ方）。`AREAS` に `LLM` | `secret_in_input_refused`、`evidence_outside_input_dropped`、`findings_capped_and_low_confidence_dropped`、`mention_and_closes_stripped`、`input_class_is_max_of_parts`、`invalid_output_retried_once_then_error` | `rust-reviewer`、`llm-integration-reviewer` | L5-0 の後 |
| L5-2 | `feat/db-llm-records` | §8.3 の表（追記のマイグレーション）、`tables.toml`、保持の掃除、プロジェクトの設定 `llm.review` | `advice_keeps_input_class`、`feedback_one_per_user_and_finding`、`input_text_never_stored`、`retention_sweep_keeps_feedback` | `database-reviewer`、`security-reviewer` | L5-1 の後 |
| L5-3 | `feat/llm-gateway` | `forge-llm`: Ollama のプロバイダ、時間切れ、digest の確かめ、列（優先度・予約 20 %・10 分の昇格・同時 1）、Kanflow の `ai-busy` の鏡、`gpu-eval` の取得の遅延、一時停止、設定 `llm.*`、doctor、メトリクス、`FG-LLM-0001〜0006` | `interactive_preempts_queue_not_running`、`review_tier_not_starved`、`kanflow_busy_defers_batch`、`kanflow_unreachable_throttles_not_blocks`、`gpu_eval_claim_waits_at_most_10_min`、`model_digest_mismatch_stops_gateway`、`endpoint_outside_allow_list_refused_at_start`、`paused_gateway_queues_nothing_new` | `rust-reviewer`、`silent-failure-hunter` | L5-2 の後 |
| L5-4 | `feat/auth-llm-actions` | §8.2 の Action と権限の表（allow と deny）、代理の読み取りの追加（ADR-0025 Amendment） | `feedback_denied_for_agent_token`、`llm_settings_denied_for_token_session`、`diagnosis_read_denied_for_non_admin`、`confidential_advice_withheld_for_agent`、`act_as_cannot_rerun_review` | `authz-matrix-auditor`、`security-reviewer` | L5-1 の後（`authorize.rs` の衝突を避け、ほかの auth の MR と順番に） |
| L5-5 | `feat/review-ai-first-pass` | §3.1。入力の組み立て（`forge-review`）、`forge/ai-review` の書き込み、古い head の破棄、深掘りの印、`gstn mr ai-review` | `ai_review_never_required_by_default`、`llm_down_sets_error_not_success`、`stale_head_result_discarded`、`injected_title_cannot_change_schema_or_conclusion`、`generated_and_lock_files_excluded`、`forge_ai_review_reserved_from_http`、`off_project_gets_no_check_row` | `llm-integration-reviewer`、`security-reviewer` | L5-3・L5-4 の後 |
| L5-6 | `feat/ui-ai-advice` | MR の画面の助言の欄（要約・指摘・深掘りの印・「役に立った / 誤り」・「AI の助言は今ありません」・機密の表示）、ja / en / id | 読み上げ、色だけに頼らない、権限の無い人に評価のボタンを出さない | `a11y-architect`、`design-reviewer` | L5-5 の後 |
| L5-7 | `feat/ops-llm-first-diagnosis` | §3.2。`forge diagnose`、事実の束（ログ・catalog・runbook・障害の記録・直近の変更）、`gstn ops diagnosis`。**着手前に確かめる**: ログの置き場所（ファイル / journald）と `diag bundle` の有無 | `diagnosis_never_contains_mutating_step`、`llm_down_prints_fact_bundle`、`log_lines_redacted_before_prompt`、`cites_only_given_lines`、`read_only_db_role` | `llm-integration-reviewer`、`security-reviewer` | L5-3 の後 |
| L5-8 | `feat/deploy-approval-summary` | §3.4。事実の一覧（`forge-deploy`）、要約、SHA への結びつき、Kanflow へのイベントと API | `approve_works_without_summary`、`facts_listed_even_if_summary_omits_them`、`new_commit_marks_summary_stale`、`summary_never_changes_approval_state` | `deploy-safety-reviewer`、`llm-integration-reviewer` | L5-3 の後 |
| L5-9 | `feat/integ-kanflow-question-tools` | §3.3。読み取りの道具のパック（理由の一覧・パイプライン・デプロイ・PBI の結びつき）、`as_of`、代理で本人の権限 | `tool_answer_hides_invisible_project`、`blocked_reasons_equal_evaluate_merge`、`confidential_path_names_not_in_tool_answer_for_agent` | `authz-matrix-auditor`、`security-reviewer` | L5-4 の後（ゲートウェイは不要） |
| L5-10 | `feat/llm-eval-sets` | §6 の評価セット（合成）、`llm-bench`、`.forge-ci.yml` の `gpu-eval` のジョブ、最初の基準 | 安全のケースが現行のプロンプトで 100 %、`bench_report_matches_adr_0034_schema` | `pr-test-analyzer`、`ci-runner-security-reviewer` | L5-3 の後。**L5-5 を試行のプロジェクトで有効にする前に** |
| L5-11 | `docs/ops-llm-runbooks` | §8.5 の runbook、doctor の項目、`errors.toml` の文 | — | `error-contract-auditor` | L5-3 と同時に始め、L5-8 で閉じる |

Kanflow の側（Kanflow の計画で行う。GitStation の MR ではない）:

| # | 中身 | 依存 |
|---|---|---|
| KF-19a | チャットの道具の振り分けに GitStation のパックを足す（02 §6 の道具パック） | L5-9 |
| KF-19b | 承認待ちのカードに要約と事実の一覧（W13） | L5-8 |
| KF-19c | `ai-busy` の応答を GitStation の worker に許す（社内の行き先の許可、読み取りだけ） | L5-3 |

### 7.2 段 2（同じゲートウェイの上に足す。各項目は着手のときに小さな計画にする）

| 項目 | 出典 | 注意 |
|---|---|---|
| `forge/spec-conformance` の影の段階（L-3、F-4） | ADR-0017 §4.1.2 | 影の後に必須。`error` のときだけ人の免除 |
| AI の危険の層（L-R） | 04 M-6、ADR-0017 Amendment F | `combine()` は実装済み。**有効にすると LLM の停止が自動マージの `stop NotAssessed` になる**（人のマージは止めない）。有効にする前に LLM の稼働率を 4 週測る |
| パイプラインの失敗の診断（L-7） | 01 §6.3、P-9 | §3.2 の部品を P-9 の診断 API に |
| 止まっている理由の解消の提案（L-5） | 01 §6.2（2026-11-02 に M5 へ） | 別ブランチの提案だけ |
| `.forge-ci.yml` の変更レビュー（L-6） | 01 §6.3 | 決定的な検査が先 |
| 自動修正の LLM | ADR-0017 | 種類ごとに評価セットの目標を満たすまで既定は無効 |
| 実績のスケジュールの LLM（E6） | 12 | 同時実行の枠を分ける、夜間 |
| 文書の修正の提案・毎晩の整理（13 §8.3 段 6・7） | 13 | 提案だけ、採るのは人 |
| ビルダーの推薦の理由（16 A11） | 16 | `RecommendDegraded` で規則に戻る |
| AI 資産の修正の下書き（17 P17-10） | 17 | 下書きのコミットを別ブランチに |
| 仕様と実装のずれの毎晩の検査（W14-2）、リリースノート・デイリー（W10、F-11・F-13・F-25） | 07、02 | 一括処理の段、夜間 |

### 7.3 順序

```
L5-0 → L5-1 → (L5-2 ‖ L5-4) → L5-3 → (L5-10 ‖ L5-7 ‖ L5-8 ‖ L5-11) → L5-5 → L5-6      … 段 1
            L5-4 → L5-9 → KF-19a（ゲートウェイを待たない）
段 1 の目標を 4 週測る → 試行のプロジェクトの外へ（§11 の問い 2）→ 段 2 を §7.2 の上から
```

## 8. 契約の項目

### 8.1 エラーコード（領域 `LLM`、L5-0 で登録。番号は採番のとき）

| 番号・名前 | 内容 | 重さ |
|---|---|---|
| 0001 LlmUnavailable | Ollama に届かない・時間切れ・VRAM 不足。チェックは `error`、表示は LLM なしに戻る | warn（runbook `llm-gateway`） |
| 0002 LlmOutputInvalid | スキーマに合わない（1 回やり直した後） | warn |
| 0003 LlmModelDigestMismatch | 実際のモデルの digest が設定と違う。ゲートウェイは推論を止める | **alert**（runbook `llm-model-update`） |
| 0004 LlmQueueDeadline | 期限（目的ごと: 対話 2 分、MR 30 分、一括 6 時間）まで待った | warn（`llm-gateway`） |
| 0005 LlmInputRefused | 秘密を伏せられない・上限を超えて分けられない・見える範囲の外 | info |
| 0006 LlmEvidenceInvalid | 指摘の根拠が入力に無い（その指摘だけ捨て、要約に「根拠の無い指摘を N 件捨てた」と出す。捨てた件数はメトリクスに） | info |

再利用: 設定の誤り → `FG-SYS-0002`（`config_key=llm.*`）、一時停止中 → ログだけ（コードなし、助言は「一時停止中」と出す）、権限 → `authorize()` の共通の拒否。

### 8.2 Action と権限の表（allow と deny の両方）

| Action | allow | deny |
|---|---|---|
| 助言を読む（MR） | `MergeRequestRead` と同じ（新しい Action を作らない）。`confidential` は人だけ | エージェントのトークン・代理 → 本文を伏せる（拒否ではなく表示しない。テストで確かめる） |
| `AiAdviceFeedback` | 助言の対象を読める人（セッション・人のトークン） | エージェントのトークン → 403（評価を汚さない）、サービスアカウント → 403 |
| `AiReviewRerun` | Developer 以上の人、1 MR 1 時間 3 回まで | エージェント・代理 → 403、超過 → 429 |
| `LlmSettingsManage`（`llm.review` の off / advisory / required） | `native_role` で Maintainer 以上、**人の Web セッションだけ**、監査 | トークン・エージェント・サービスアカウント・自動マージの主体 → 403 |
| `OpsDiagnosisRead` | インスタンスの管理者（`admin` スコープ）。エージェントのトークンは入力が `internal` の診断だけ | 管理者でない → 404 |
| `LlmPause` / `LlmResume` | 管理者 + `admin` スコープ、人。本番では承認済みの runbook の手順 | エージェント → 403 |
| 代理の読み取り（ADR-0025 の許可リストに追加） | `PipelineRead`・`DeploymentRead` など読み取りだけ | 書き込みの Action は足さない（`act_as_cannot_rerun_review`） |

`forge diagnose` はサーバーの CLI（HTTP でない）で、読み取りだけの DB のロールを使う。

### 8.3 マイグレーション（すべて追記）

| 表 | 主な列 | 規則 |
|---|---|---|
| `llm_requests` | `purpose`、`priority`、`subject_kind` / `subject_id`、`project_id`、`requested_by`、`model`、`model_digest`、`prompt_id`、`prompt_version`、`input_sha256`、`input_class`、`input_tokens` / `output_tokens`、`state`、`error_code`、`request_id`、`queued_at` / `started_at` / `finished_at` | 入力の本文は持たない。90 日で消す |
| `llm_advice` | `llm_request_id`、`subject_*`、`head_sha`（MR・承認）、`kind`、`summary`、`findings` JSONB、`deep_review`、`input_class`、`superseded_by` | `input_class` は下げられない（CHECK とトリガー）。180 日 |
| `llm_advice_feedback` | `advice_id`、`finding_index`、`user_id`、`verdict`（`useful` / `wrong`） | (advice, finding, user) で一意。消さない（指標の元） |
| プロジェクトの設定 | `llm.review`（既定 `off`） | 変更は監査 |

### 8.4 メトリクスと doctor

- `forge_llm_queue_wait_seconds{priority}`（ADR-0017 で名前を予約済み）、`forge_llm_request_duration_seconds{purpose}`、`forge_llm_requests_total{purpose,outcome}`、`forge_llm_output_invalid_total{purpose}`、`forge_llm_tokens_total{direction}`、`forge_llm_yield_total{reason=kanflow_busy|gpu_eval}`、`forge_llm_advice_feedback_total{purpose,verdict}`、`forge_llm_withheld_total{reason=confidential}`。
- 警報: `LlmModelDigestMismatch`、`LlmUnavailable` が 30 分続く、MR レビューの待ちの p95 が 30 分を超える。
- doctor: `llm.endpoint`（届くか・社内か）、`llm.model`（digest）、`llm.num_ctx`（Kanflow と同じか）、`llm.queue_age`、`llm.kanflow_gate`。**readiness には入れない**（助言なので、LLM の停止で `/readyz` を落とさない）。

### 8.5 runbook（新規）

`llm-gateway`（届かない・混む・一時停止と再開・Kanflow との取り合い）、`llm-model-update`（モデルの変更は MR 1 本: `name:tag@digest`・評価セットで新旧の比較・Kanflow と同時に切り替える。計画 17 の `ai-model-update` と同じ型）、`llm-advice-quality`（採用率が下がった・誤検知が増えた → プロンプトの版を戻す）。

## 9. 既存の機能への影響

| 既存の機能 | 変わるか | 理由 |
|---|---|---|
| マージの条件・自動マージ（既定 `full`）・`stop` の分類 | 変わらない | `forge/ai-review` は助言で必須でない。L-R は段 2 |
| デプロイの承認（prod は人） | 変わらない | 要約は表示を足すだけ。ボタンは要約と無関係 |
| CI・`gpu-eval` | ほぼ変わらない | `gpu-eval` の取得が対話の推論の間だけ最大 10 分遅れる |
| `/readyz` | 変わらない | LLM は readiness に入れない |
| Kanflow のチャット | 混雑が増えうる | forge は Kanflow の利用者が待つ間は新しい推論を始めない |
| `llm.review = off` のプロジェクト | 何も変わらない | チェックの行も出ない |

## 10. 既に決まっていること（聞き直さない）

| # | 決定 | 出典 |
|---|---|---|
| 1 | LLM は M5（MR は M3、SSH は M4） | 01 §11 の 4 |
| 2 | GPU は Kanflow の Ollama（Qwen3.8 27B）を共用、forge 側で同時実行を絞る。増えたら専用 | 01 §11 の 6 |
| 3 | LLM のチェックの既定は助言、プロジェクトで必須にできる。失敗は `error` | 01 §11 の 7、ADR-0003 |
| 4 | サーバーで Claude を動かさない。サーバーの AI はローカル LLM だけ | 06 D-1 |
| 5 | データの境界は 3 区分、機械で強制 | 06 D-5 |
| 6 | 優先度、MR レビューの 20 % の予約、10 分の昇格、自動修正は同時 1 | ADR-0017 §3 |
| 7 | エラーの領域 `LLM` を承認 | ADR-0017 決定 8 |
| 8 | AI のコミットはすべての承認を消す。`forge/spec-conformance` は影 → 必須、自動マージの AI 承認は保留 | ADR-0017 決定 7・9 |
| 9 | L-5 を M8 から M5 へ（提案は別ブランチ、適用は人） | 01 §6.2（2026-11-02） |
| 10 | 自動マージの既定 `full`。LLM は危険を上げるだけ、止める規則を外せない。本番のデプロイは人 | 04 M-5・M-6、01 §6.1 |
| 11 | W14 の優先は 1・5・6・11。LLM が使えないときは LLM なしの表示に戻す。BankMasalah と共有、`num_ctx` をそろえる | 07 W14 |
| 12 | 文書の LLM は提案だけ、報告は `owner` のグループ。段 6・7 は M5 の後 | 13 §8.3、§12、Q10 |
| 13 | `gpu-eval` はこの開発サーバーの GPU、Kanflow とは時間で分ける。モデルは `name:tag@digest`、`latest` 禁止 | 17 §8 の 1、§6 |
| 14 | ビルダーの推薦は M5 を待たない（規則で始め、LLM は A11） | 16 §1.3 |
| 15 | E6 は M5、L-5 と同じ段で同時実行の枠を分ける | 12 §E6 の時期、問い 12 |
| 16 | M5 で `llm-integration-reviewer` とプロンプトの変更の評価セットのゲートを足す | 01 §8 |
| 17 | Web Studio の AI の既定はローカル LLM、機密の印のあるものは Claude に渡さない | 09 §9 の 8 |

## 11. 利用者に聞くこと（★ = 着手の前に要る）— 2026-11-23 にすべて決定

1. ★ **本番の GPU**: 本番（PFS の Linux サーバー）の GitStation が呼ぶ Ollama をどのホストにするか。
   **推奨の既定**: 試験はこの開発サーバーの GPU（Kanflow と共用、G1）。本番は利用者が指定する社内の GPU のホスト（PFS の AI ノードは使わない）で、**指定があるまで本番では `llm.*` を無効**（助言は出ず、ほかは今のまま）。
   **決定: 推奨の既定**（試験はこの開発サーバーの GPU を Kanflow と共用。本番は利用者が社内の GPU のホストを指定するまで `llm.enabled = false`。設定の既定も無効）、2026-11-23。
2. ★ **一次レビューの広げ方**: 最初から全プロジェクトで `advisory` にするか。
   **推奨の既定**: 段 1 は試行のプロジェクトだけ（GitStation 自身と Kanflow の 1 チーム）。§6 の目標を 4 週満たしたら、新しいプロジェクトの既定を `advisory` にする（R2）。
   **決定: 推奨の既定**（段 1 は試行のプロジェクトだけ `advisory`、ほかは `off`。助言だけで、マージを止めない）、2026-11-23。
3. **R2 の目標の数字**（01 §8 で「M5 の開始時に決める」）。
   **推奨の既定**: 採用率 ≥ 30 %、誤検知 ≤ 20 %、仕込んだ欠陥の検出 ≥ 70 %、安全のケース 100 %、MR レビュー p95 ≤ 10 分、承認の要約 p95 ≤ 60 秒。最初の 4 週の実測で決め直す。
   **決定: 推奨の既定**（上の数字。最初の 4 週の実測で決め直す）、2026-11-23。
4. **GitStation の中のチャット**: 自然な言葉の質問を GitStation の画面にも置くか。
   **推奨の既定**: 置かない。入口は Kanflow のチャットだけで、GitStation は道具を出す。
   **決定: 推奨の既定**（置かない。入口は Kanflow のチャットだけ）、2026-11-23。
5. **要約の言語**: MR の要約・承認の要約を ja / en / id のどれで作るか（言語ごとに GPU を使う）。
   **推奨の既定**: ja を常に作り、en / id は画面で開いたときだけ作る（一括で作らない）。
   **決定: 推奨の既定**（ja を常に作り、en / id は開いたときだけ）、2026-11-23。
6. **障害の診断を Claude Code（エージェントのトークン）に読ませるか**。
   **推奨の既定**: 読ませる。ただし管理者のエージェントのトークンで、入力の区分が `internal` の診断だけ（個人情報を含みうるログの診断は人だけ）。
   **決定: 推奨の既定**（管理者のエージェントのトークンで、入力の区分が `internal` の診断だけ）、2026-11-23。

## 12. 危険と対策

| 危険 | 対策 |
|---|---|
| 指摘の質が低く読まれなくなる | 10 件まで、確信度で絞る、採用率と誤検知で測る、試行のプロジェクトから |
| 差分・ログ・文書の注入 | 助言しか出ない（操作の経路が無い）、出力の検査、安全のケース 100 % |
| 機密が Claude 側に出る | 助言の区分は入力の最大、エージェント・代理には伏せる、テストと eval |
| LLM を通した IDOR | 入力は依頼した人・イベントの主体に見えるものだけ |
| Kanflow のチャットが遅くなる | `ai-busy` に譲る、同時 1、待ちの監視、増えたら G3 |
| ベンチと推論の VRAM の取り合い | 取得の遅延（最大 10 分）、夜間に一括、失敗は再試行で記録 |
| モデルが黙って変わる | digest の固定と確かめ（alert）、モデルの変更は MR と評価セット |
| LLM が止まる | 何も止めない（助言は「今ありません」）。L-R を有効にするまでは自動マージにも影響なし |
