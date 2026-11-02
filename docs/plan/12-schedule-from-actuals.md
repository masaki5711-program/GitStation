# 12 実績から作るスケジュール — ベースの計画に、過去の実績と反省点を落とし込み、負荷を配り直す

作成: 2026-11-02（planner）。状態: **計画に盛り込み済み（2026-11-02、利用者の指示「ロジックも含めて作って計画に盛り込んでください」）**。§9 の答えは各 MR の着手前に聞く。E0 は 2026-11-02 に着手。
参照: 01 §3（原則 3）・§6・§8、02 F-7・F-14・F-17・F-24・F-33・§5.2・§5.7、03 §7・§8.1、04 C-3、07 W12・W14-9、08、09 §2、11 §5.6・§5.7（X1〜X3）、ADR-0023。Kanflow v2（worktree `D:\APP\Kanflow-v2`）: `app/prisma/schema.prisma`、`app/src/services/plan/`、`app/src/lib/plan-schedule/base-schedule.ts`、`app/src/lib/plan-templates/schedule.ts`、`app/src/lib/plan-progress.ts`、`app/src/lib/chat-proposals.ts`、`app/src/app/api/mcp/_lib/plan-tools.ts`、`app/src/lib/authz/policy.ts`、`docs/plan/team-dev-implementation.md` §9・§10、`docs/plan/ux-simplicity.md` §1。

## 0. 前提の点検

### 0.1 目標（1 文）

新しいプロジェクトの全体計画を、ベースのスケジュール（テンプレートの `schedule.yml`）を骨組みにして作り、過去に終わったプロジェクトの実績と反省点から**各タスクの期間と担当者の負荷を配り直した案**を、根拠付きで PO / PM に提案する。承認するまで計画は変わらず、データが少ないうちはテンプレートどおりに戻る。

### 0.2 利用者の意図（2026-11-02、原文の要点）

- 「AI が過去の実績から生成する仕組み」。「テンプレートからでも OK」。
- 「過去のものをただコピペするのではなく、反省点も踏まえて各タスクの負荷を再分配する」。
- 「ベースとなる計画に、過去の実績を落とし込んでいく」。「ロジックも含めて計画に盛り込む」。
- 同じ日の追記: **リポジトリをまたぐ連携は GitStation の大きな差別化の点。** `.forge/repo.yml` で固定した仕様の参照、`spec-drift` の通知、PBI ごとの MR セットとマージの順序（04 C-3）、1 つの Kanflow のプロジェクトに複数のリポジトリを結ぶこと。

この計画では次のように読む。**骨組みは常にベースのスケジュール。** 過去の計画の木を写すことはしない。過去から取り込むのは、(1) テンプレートの項目（`key`）ごとの「予定と実績の比」の統計、(2) 確定した反省点の規則（決まった種類の構造の変更）、(3) 人ごとの同時に持てる量、の 3 つだけ。**実績の単位は Kanflow のプロジェクトで、結んだリポジトリが何本あっても 1 つに集計する。**

### 0.3 コードで確かめたこと（Kanflow v2）

| # | 事実 | 出典 |
|---|---|---|
| 1 | `PlanItem` には予定（`plannedStart/End`）・実績（`actualStart/End`）・基準（`baselineStart/End`）・`durationDays`・担当 1 人（`ownerId`）・`status`・`templateApplicationId` がある。**テンプレートの `key` を持つ列は無い** | `schema.prisma` 72〜125 行 |
| 2 | `status` は `not_started`・`in_progress`・`done`・`on_hold` の 4 つ。遅れは列に保存せず、その都度計算する | `plan-progress.ts` 38〜44 行 |
| 3 | `loadBaseSchedule` は行・依存・進み具合を 1 つのトランザクションで作る。ただし **`key` を保存しない**。**基準（baseline）も置かない**。どのスケジュールから作ったかは `ActivityLog` の印（`proposalId`）が残るだけ | `plan.base-schedule.ts` 41〜57 行、`plan.repository.ts` 283〜293 行 |
| 4 | `BaseScheduleItem` の項目は `key`・`name`・`durationDays`・`milestone`・`dependsOn`・`children` だけ。**役割も負荷も持たない**。上限は 500 件・5 段・3650 日 | `base-schedule.ts` 14〜34 行 |
| 5 | 実績の日付: PBI に結んだ葉は、PBI の状態から空欄だけを今日の日付で埋める。手動の葉は人が入れる（入力の漏れの程度は**未確認**） | `plan-progress.ts` 303〜317 行 |
| 6 | 基準は「基準を保存」の操作で予定の日付を写す（生の SQL） | `plan.repository.ts` 245 行 |
| 7 | 作業量の実績はスプリントのタスク（`estimatedHours`・`actualHours`・`TaskAssignee`）にある。PBI は 1 つの葉に結べる（`planItemId`）。PBI には機密の印 `confidential` がある（D-5） | `schema.prisma` 532〜590 行・773〜805 行・1166 行 |
| 8 | 振り返りの文はスプリントの `retrospectiveKeep/Problem/Try` にある。W11 の `Lesson` は開発の失敗の署名（signature）が鍵で、`scope` は `global`・`tech:`・`project:` のどれか。`confidential` がある。**計画の `key` や設備・工程のマスタに結ぶ列は無い** | `schema.prisma` 742〜745 行・1817〜1849 行 |
| 9 | 設備・工程のマスタの表は Kanflow に無い（team-dev §9 は「教訓にマスタの ID を足す」と書くが未実装）。マスタの正本の場所は**未確認** | `schema.prisma` を grep、team-dev §9 |
| 10 | 人ごとの稼働量・休み・会社の休日の表は無い。`Project.defaultCapacity` と `Sprint.capacity` はポイントの単位。日付の計算 `scheduleFrom` は土日を飛ばす設定だけを持ち、祝日と「この日より前に始めない」の指定は無い | `schema.prisma` 282・737 行、`plan-templates/schedule.ts` 58〜147 行 |
| 11 | 承認の流れ: MCP `propose_plan_items`（スコープ `backlog:write`）→ チャットの承認カード `plan_load` → `POST /plan/load` が日付をサーバーで計算し直す。カードは 55,000 バイトまで、プレビューは 60 行まで | `plan-tools.ts`、`plan.propose.ts` 29 行、`chat-proposals.ts` 176〜188 行 |
| 12 | 権限: `plan.write` は PO / SM。判定は `policy.ts` の 1 か所 | `policy.ts` 105〜137 行 |
| 13 | `GitLink` は MR の `state` と `mergedAt` を持つ。開いた日時・差分の大きさ・版の数は持たない | `schema.prisma` 1420〜1461 行 |
| 14 | 同じリポジトリの計画テンプレート（`PlanTemplate`）の段は `key` を任意で持つ。`PlanTemplateApplication.content` は読み込んだ時点の内容を保存する | `plan-templates/schema.ts` 63 行、`schema.prisma` 180〜203 行 |
| 15 | **1 つのプロジェクトに複数のリポジトリを結べる。** `Project.gitRepositories` がある。`GitLink` には `repositoryId` があり、一意の鍵は `(repositoryId, kind, ref, backlogItemId)` なので、1 つの PBI が複数のリポジトリの MR を持てる。読む先（`GitIntegration`）はプロジェクトに 1 つ | `schema.prisma` 330〜332 行・1296 行・1318 行〜・1453〜1457 行 |

GitStation 側: 開発状況 API（02 §5.2）は M3 の I1 で作る。応答の MR には `project` の欄があり、リポジトリをまたいで返す。`/metrics/flow`（F-14）は第 2 段（M4）。F-17 と F-33 は Could で第 3 段。リポジトリをまたぐ連携 (a)〜(c) は M3 後半（11 §5.7 の X1〜X3）。MR セットのマージの順序（C-3）は M4。

### 0.4 前提の点検（提案された形は合っているか）

1. **「AI が生成する」をそのまま作ると目的に反する。** LLM が過去の計画を読んで新しい計画を書くと、写しに近くなり、数字の根拠も追えず、試験もできない。01 §3 の原則 3（決定的な処理が主、LLM は補助）と 07 W14 に合わせ、**期間と負荷の数字はコードで決め、LLM は反省点の文を規則の下書きにする役と説明の文に限る**。LLM が無くても機能の全体が動く（§5）。
2. **負荷の配り直しは、過去のデータが無くても初日から役に立つ。** テンプレートの期間のままでも、人の掛け持ちが重なれば平準化できる。データを貯める部分（E0）と平準化（E4）を、LLM（M5）より先に出す価値がある。
3. **今のままではデータが貯まらない。** `key` と基準が保存されていない（§0.3 の 1・3）。**E0（`key` と基準の保存）は今すぐ入れる**のが最も効く。
4. **計画の持ち主は Kanflow。** 計算と保存は Kanflow で行う。GitStation が出すのは、ソフトウェアのプロジェクトの MR の事実（リードタイム・規模・手戻り）だけで、必須ではない（設備・工程には MR が無い）。**MR の事実は「PBI ごとの MR セット」を単位に、プロジェクトに結んだ全リポジトリをまたいで集計する（§2.4）。**
5. **費用**: 新しい表が 8 つ、MR が Kanflow で 7 本・GitStation で 1 本。攻撃面は「他のプロジェクトの数字が見える」ことと「他の人の負荷が見える」こと（§4）。

## 1. 選択肢と推奨

| 軸 | A: LLM が過去の計画から直接作る | **B: ベース + 統計の見積もり + 規則 + 平準化。LLM は補助（推奨）** | C: テンプレートだけ + 振り返りの表（F-17） |
|---|---|---|---|
| 内容 | 似たプロジェクトの計画と振り返りを LLM に渡し、新しいスケジュールを出させる | テンプレートの木はそのまま。葉ごとに実績の比で期間を補正し、確定した反省の規則を当て、人の負荷で平準化する。LLM は規則の下書き・あいまいな照合の補助・説明の文 | 見積もりと実績の差を表で見せるだけ |
| 「写さない」 | 守れない | 守れる | 守れる |
| 負荷の再配分 | 根拠が無い | 決定的に計算し、試験できる | 無い |
| データが無いとき | 出力が不安定 | テンプレートに戻る。平準化は効く | そのまま |
| LLM が止まったとき | 機能が止まる | 決定的な部分は動く | 影響なし |
| 評価 | 難しい | バックテストで MAPE と P80 の当たりの率を測れる | 不要 |

**判断: B。** C の振り返りの表は B の評価の画面（§5）に含める。平準化の解き方は **B-1: 優先度つきの逐次の割り付け（SSGS）**（クリティカルパスが先、余裕の小さい順に人の空きに詰める。TypeScript だけで書け、同じ入力なら同じ出力、結果を説明できる）。B-2 の制約ソルバー（CP-SAT など）は説明しにくく、同じ答えの保証が弱いので採らない。

## 2. 設計 — データ（Kanflow、PostgreSQL 16、追記のマイグレーション）

### 2.1 今あるものと足すもの

| 要るもの | 今ある? | 足すもの（MR） |
|---|---|---|
| 葉がテンプレートのどの項目か | **無い** | `PlanItem.templateKey`（varchar 40、null 可）と `PlanItem.scheduleLoadId`（E0） |
| どのスケジュールを何日に読み込んだか | `ActivityLog` の印だけ | 表 `plan_schedule_load`: `projectId`、`proposalId`、`sourceRef`（例 `gitstation:<repo>@<tag>:schedule.yml`。X1 の後は固定の解決の結果を写す。または `template:<id>@v<n>`）、`lineage`（テンプレートの系統の名前。版が変わっても同じ）、`scheduleHash`、`content`、`startDate`、`skipWeekends`、`estimateRunId`（null = テンプレートだけ）、`loadedById`、`loadedAt`（E0） |
| 承認した時点の予定 | 列はある。読み込みでは置かない | `/plan/load` で基準を予定と同じ日付にする（E0） |
| 役割と負荷の目安 | **無い** | `BaseScheduleItem` に任意の項目: `role`、`load`（0〜1。0 = 納期待ちなど人が動かない期間、既定 1）、`formerKeys`（改名したときの旧名）。古い `schedule.yml` もそのまま通る（E0） |
| 終わったプロジェクトの実績 | 行に散らばっている | 表 `plan_actuals_snapshot` と `plan_actuals_item`（§2.2、E1） |
| リポジトリごとの MR の内訳 | `GitLink`（時刻と規模は無い） | 表 `plan_actuals_item_repo`（§2.4、E1） |
| 反省点 → 計画の変更 | 文だけ | 表 `plan_lesson_rule`（§2.3、E3） |
| 人の稼働量 | **無い** | 表 `member_capacity`（`userId`、`from`、`to`、`fte` 0〜1、`note`）。無ければ既定の 1.0（E4） |
| 見積もりの実行と根拠 | 無い | 表 `plan_estimate_run`（入力のハッシュ・方式の版・項目ごとの根拠・平準化の結果・LLM のモデルとプロンプトの版）（E5） |
| 利用者の反応 | 無い | 表 `plan_estimate_feedback`（`runId`、`key`、`kind` = `too_high`・`too_low`・`rule_not_applicable`・`other`、`note`、`userId`）（E5） |
| バックテストの記録 | 無い | 表 `plan_estimate_eval`（系統・評価の日・n・MAPE・P80 の当たりの率・終了日の誤差・方式の版）（E2） |
| 設備・工程のマスタの ID | **無い** | 表 `project_attribute`（`projectId`、`kind` = `equipment`・`process`・`tag`、`ref`）。正本が決まるまでは自由な文字列（Q5）（E1） |
| MR の事実（ソフトウェアだけ） | `mergedAt` だけ | GitStation の G1 で開発状況 API に項目を足す。Kanflow は `GitLink` に `openedAt`・`versionCount`・`additions`・`deletions` を足す（E1） |

### 2.2 実績のスナップショット（E1）

**作る時**: PO / SM が「実績を確定」を押した時（新しい操作 `plan.actualsFinalize`）。プロジェクトを完了にした時は催促を 1 回出す。確定し直しは新しい行を足し、古い行に `supersededAt` を入れる（行を書き換えない）。

**理由**: 計画は後から直される・ゴミ箱に入る・リポジトリの結び付きが外れる。バックテストを同じ結果で繰り返すには、その時点の値を凍結する。他のプロジェクトの負荷も、集計した数字だけを残せる。

`plan_actuals_snapshot`: `projectId`、`projectType`、`lineage`、`scheduleHash`、`skipWeekends`、`closedAt`、`repoCount`、`includeInLearning`（既定 true。PO / SM が外せる。理由を残す）、`version`、`supersededAt`。

`plan_actuals_item`（葉だけ）:

| 列 | 意味・求め方 |
|---|---|
| `templateKey`、`name`、`parentKey`、`isMilestone` | 照合の鍵と表示 |
| `templateDays` | 読み込んだ時点の `durationDays` |
| `plannedDays` | 基準の日数（無ければ予定）。数え方は `skipWeekends` に合わせる |
| `actualDays` | `actualStart`〜`actualEnd`（両端を含む）。どちらかが空なら学習に使わない（`complete = false`） |
| `startSlipDays` / `endSlipDays` | 実績 − 基準 |
| `waitGapDays` | max(0, 自分の実績の開始 − 先行の実績の終了の最大 − 1) |
| `onHoldDays` | `ActivityLog` から復元した `on_hold` の日数（**記録が必ず残るかは未確認**。残らないなら null） |
| `ownerId` | 担当 |
| `ownerConcurrentAvg` / `ownerConcurrentPeak` | 期間中に担当が同時に持っていた葉の数（全プロジェクト）。**他のプロジェクトの名前は残さない** |
| `effortHours` / `estimatedHours` | 結んだ PBI のタスクの実績・見積の時間（ソフトウェア）。機密の PBI の分は入れない |
| `loadObserved` | effortHours ÷ (actualDays × 1 日の時間（既定 7.5、Q1））。0〜1.5 に収める |
| `reworkCount` | PBI の再オープン + **全リポジトリの** MR の再オープンと revert |
| `mrSetCount`、`mrCount`、`repoTouched` | MR セットの数、MR の数（全リポジトリ）、触ったリポジトリの数 |
| `mrSetLeadTimeP50Hours`、`mrLeadTimeP50Hours` | MR セットと MR 1 本ずつのリードタイムの中央値（無ければ null） |
| `mergeOrderViolations` | C-3 の順序の警告の数（C-3 の後。それまでは null） |
| `confidential` | 結んだ PBI がすべて機密なら true（学習の対象外） |

**確定の前の検査**（決定的）: 実績の日付が空の葉、開始 > 終了、実績が基準の 5 倍を超える葉、読めなかったリポジトリを一覧で示す（`PLAN_ACTUALS_INCOMPLETE`）。MR の欄は null にする（0 にしない）。それでも確定はでき、問題の行は `complete = false`。

### 2.3 反省点の規則（E3）

規則は**決まった種類だけ**。LLM も人も、この形でしか計画を変えられない。

| 種類 | 引数 | 上限（検証） |
|---|---|---|
| `scale_duration` | `key`、`factor` | 0.5〜3.0 |
| `add_buffer` | `afterKey`、`days` | 1〜30 日、`load = 0` |
| `add_prerequisite` | `beforeKey`、`newKey`、`name`、`durationDays`、`role?`、`load?` | 新しい `key` は重複なし、1 回の提案で 30 件まで |
| `add_dependency` | `from`、`to` | 循環・自分・祖先は不可 |
| `split` | `key`、`parts` | 2〜4。1 つあたり 2 日以上 |
| `cap_parallel` | `role`、`max` | 1〜5 |

表 `plan_lesson_rule`: `kind`、`params`（zod で検証）、範囲（`lineage`・`projectType`・`project_attribute` の ref のどれか 1 つ以上）、`state` = `draft` → `confirmed` → `retired`、出典（`sourceProjectId`、`lessonId?`、`sprintId?`、引用した文）、`confidential`、`createdBy` = `human` / `llm`、`confirmedById`、`confirmedAt`。**計画に効くのは `confirmed` だけ。** W11 の `Lesson` は開発の失敗の署名が鍵なので使い回さない。

### 2.4 複数のリポジトリを結んだプロジェクト（MR の事実の集計）

- **単位は PBI ごとの MR セット**（02 F-24、04 C-3）: 同じ PBI を参照する、結んだ全リポジトリの MR。マージしないで閉じた MR は数えない。
- **MR セットのリードタイム** = セットで最後にマージされた `merged_at` − 最初に開いた `opened_at`。PBI がリポジトリの数だけ分かれても 1 つの作業の時間として学べる。参考に MR 1 本ずつの中央値も残す。
- **表 `plan_actuals_item_repo`**: `snapshotItemId`、`repositoryRef`（GitStation のプロジェクトのパスと表示名）、`mrCount`、`leadTimeP50Hours`、`additions`、`deletions`、`versionCount`、`reworkCount`。葉の行はこの合計か中央値。
- **リポジトリの数の違い**は葉の期間の比に直接入れない。`repoTouched` を根拠に出し、E2 のバックテストで「数で分けたほうが当たる」と分かったら照合の似ている度に係数を足す（方式の版を上げる）。
- **取り込み**: Kanflow の GitStationSource（11 K3）は結んだ全リポジトリのイベントを 1 つのカーソルで読む。スナップショットは `GitLink` から作り、確定の時点の値で凍結する。開発状況 API を呼ぶのは欠けを埋めるときだけ。
- **仕様の版**: `sourceRef` にベースのリポジトリの固定した版（X1）を写す。`spec-drift`（X2）で仕様が改訂されたプロジェクトは `specRevisedDuringProject` を残し、見積もりには使わず根拠に出す（Q11）。

## 3. 設計 — ロジック（決定的なものが先、LLM は後）

記号: 葉 i の `key` を k_i、テンプレートの日数を T_i、役割を r_i、負荷を ℓ_i。過去の観測 j の予定の日数を P_j、実績を A_j。対象は葉だけで、マイルストーンは 1 日に固定する。

### 3.1 照合（過去の観測を集める）

対象は `includeInLearning = true` かつ `complete = true` かつ `confidential = false` で、**`closedAt` < 新しいプロジェクトの開始日**のもの（バックテストと同じ条件で先の情報を漏らさない）。

| 段 | 条件 | 似ている度 s_j |
|---|---|---|
| 1 | 同じ `lineage` で同じ `key`（または `formerKeys`） | 1.0 |
| 2 | 系統は違うが同じ `projectType` で同じ `key` | 0.7 |
| 3 | 同じ `projectType` で `project_attribute` の ref が 1 つ以上同じで、名前が似ている（`pg_trgm` similarity ≥ 0.6） | 0.5 × similarity |
| 4 | 同じ `projectType` で名前が似ている（≥ 0.75） | 0.3 × similarity |

段 1 の観測が 3 件以上あれば段 3・4 は使わない。段 3・4 の採否は E6（M5）から LLM が補助する（返すのは候補の ID の部分集合だけで、似ている度は式のまま）。

重み: w_j = s_j × 0.5^(経過日数_j / H)。H は半減期（既定 365 日、Q7）。

### 3.2 期間の見積もり（P50 / P80）

対数の比 x_j = ln(A_j / P_j) を使う（右に裾が長い分布に頑健。比を今の T_i に掛けるので、テンプレートの版の違いにも影響されない）。

```
estimate(i):
  obs = match(i)                                    # §3.1
  if obs is empty: return prior(i)                  # §3.3
  m   = weighted_median(x, w)
  mad = max(weighted_median(|x - m|, w), 0.05)
  kept = obs where |x_j - m| <= 3 * 1.4826 * mad    # 外れ値を除く（除いた件数は根拠に出す）
  m*  = weighted_median(x[kept], w[kept])
  n   = (Σw)^2 / Σw^2   over kept                   # 有効な件数
  μ0  = prior_logratio(projectType)                 # §3.3
  α   = n / (n + K)                                 # K = 3（Q7）
  x̂   = α * m* + (1 - α) * μ0                       # テンプレート側へ縮める
  s80 = weighted_quantile(x[kept], w[kept], 0.8) - m*   (n >= 3 のときだけ。それ以外は S0)
  ŝ   = α * max(s80, 0) + (1 - α) * S0              # S0 = 0.25（約 +28%）
  r50 = clamp(exp(x̂), RMIN, RMAX)                   # 0.5〜3.0
  D50 = clamp(ceil(T_i * r50), 1, 3650)
  D80 = clamp(ceil(T_i * clamp(exp(x̂ + ŝ), RMIN, RMAX)), D50, 3650)
  lag = shrink(weighted_median(waitGapDays), toward 0, K)   # 1 日以上のときだけ「待ち」を提案
  return { D50, D80, n, α, r50, kept, dropped, sources, lag,
           mrSet: { leadTimeP50, repoTouchedP50 } }  # ソフトウェアだけ。根拠の表示用
```

- **プロジェクト全体のバッファ**（各葉に余裕を足さない）: クリティカルパス上の葉について B = ceil( sqrt( Σ (D80_i − D50_i)^2 ) )。カードに「P50 の終了日」と「P80 の終了日 = P50 + B」を並べる。行として入れるかは Q4。
- 負荷の学習（ソフトウェア）: `loadObserved` の重みつき中央値を ℓ_i の方向に同じ α で縮めて ℓ̂_i とする。
- MR セットのリードタイムは期間の見積もりには使わず、F-33 の参考情報と根拠の表示に使う。

### 3.3 データが無い・少ないとき

| 状態 | 動き |
|---|---|
| 観測が無く、種類の観測も無い | μ0 = 0。D50 = T_i（**テンプレートどおり**）。D80 = T_i × e^0.25 |
| 項目の観測は無いが、同じ種類の観測は N 件ある | μ0 = (N/(N+10)) × 種類の全観測の重みつき中央値。印「種類の傾向だけ」 |
| 項目の観測が少ない | α が小さく、ほぼ μ0 に寄る |
| 反省の規則が無い | 構造はテンプレートのまま |
| 稼働量の表が空 | 全員 1.0 FTE、同時に持つ数の上限 m_u = 2（Q1） |
| MR の事実が無い | MR の欄は null、根拠に「MR の情報なし」。期間の見積もりには影響しない |
| LLM が止まっている・時間切れ・出力が不正 | 決定的な部分だけで提案し、「AI の補助なし」と 1 行出す（失敗は記録し、成功扱いにしない） |

### 3.4 反省点の適用と検証

```
apply_rules(template, rules_confirmed_matching_scope):
  s = deep_copy(template)
  for rule in sort(rules, by kind order: add_prerequisite, add_dependency, split, add_buffer, scale_duration, cap_parallel; then by id):
    s = apply(rule, s)                    # 種類ごとに 1 つの純粋な関数
  validate(s):                            # 違反があればその規則を外し、理由を根拠に出す
    planBaseSchedule(s, startDate) が通る   # 500 件・5 段・3650 日・key の重複・循環・祖先（既存の検証を再利用）
    追加の葉 <= 30 件 かつ <= テンプレートの 20%
    1 つの葉の factor の積は 0.5〜3.0
  return s, applied[], rejected[]
```

`scale_duration` は §3.2 の r50 に掛ける（統計と規則の二重の補正の上限は上の積の範囲）。

**LLM の役割（E6、M5）**:
1. 実績の確定の時、スプリントの振り返りの文（Problem / Try）、`project:<id>` の確定した教訓、確定の時のメモを読み、§2.3 の形の `draft` の規則を出す。各規則に**引用した文と出典の ID**を付ける。zod で検証し、不正なら 1 回だけ再試行し、それでも不正なら捨てて記録する。PO / SM が確定するまで効かない。
2. 提案の時、§3.1 の段 3・4 の候補の採否を補助し、根拠の説明の文（ja / en / id）を書く。
3. LLM が数字を出す経路は作らない。入力はすべてデータとして扱い（振り返りの文にプロンプトインジェクションが入りうる）、道具は渡さない（01 §6.1）。

### 3.5 負荷の再配分（平準化、B-1）

入力: 規則を当てた後の木、葉ごとの D50 と ℓ̂、役割の割り当て `roleAssignments: { role: userId[] }`（提案の時に PO / PM が選ぶ。`role` が無い葉は「担当未定」）、稼働量 C_u(d) = `member_capacity` の FTE − u が**他のプロジェクトで**その日に持つ葉の ℓ の合計（0 未満は 0）、同時に持つ数の上限 m_u（過去の `ownerConcurrentPeak` の 80 パーセンタイルを既定 2 の方向に K で縮めて丸める。`cap_parallel` があればそれ）。

```
level(items, D, ℓ, assign, C, m):
  cpm = forward/backward pass with D (lag は先行の終了に足す)   # ES, EF, LS, LF, 余裕 TF
  used[u][d] = 0, count[u][d] = 0
  eligible = 先行がすべて決まった葉
  while eligible:
    i = argmin over eligible of (TF_i, LF_i, -後続の鎖の長さ_i, テンプレートの並び順)   # クリティカルパスが先
    t0 = max(先行の終了 + 1 + lag_i, 開始日)
    cands = assign[r_i] or [unassigned]
    for u in cands:
      t_u = 最も早い t >= t0 で、[t, t+D_i-1] の全稼働日に used[u][d]+ℓ_i <= C_u(d) かつ count[u][d] < m_u
    choose u* = argmin (t_u + D_i, 割り付け済みの負荷の合計_u, 親の葉と同じ担当か[同じなら優先], userId)
    if t_u* > LS_i and split 可能(規則 split または D_i >= 4 かつ候補 2 人以上):
      2 つに分けた案（各 ceil(D_i/2 × 1.1)、別の人）を試し、終了が早ければ「分割の提案」として採る
    確定: start_i, end_i, owner_i = u*; used と count を足す
  return schedule, owner, overload_before, overload_after, moves[]
```

- `ℓ = 0` の葉（納期待ちなど）は人の枠を使わない。
- **比較**: テンプレートの日数に担当を割り当てただけの配置（before）と平準化の後（after）の、人ごとの最大の使用率と 1.0 を超える日数。誰の何をなぜ動かしたかの一覧（`moves`）。
- **人の割り当ての変更は案に留める。** カードで 1 件ずつ外せる。
- 同じ入力なら同じ結果（決め手に `userId` とテンプレートの並び順）。
- 負荷はリポジトリではなく人と葉で数える。

### 3.6 出力と承認（既存の流れを使い回す）

- 平準化の結果は**日付ではなく制約として**スケジュールに書く: `BaseScheduleItem` に任意の `notBeforeOffsetDays` と `ownerId` を足し、`scheduleFrom` に「この日より前に始めない」を足す（E4）。承認の時の `POST /plan/load` は今までどおりサーバーで日付を計算し直すだけで、カードの日付を信用しない約束を守れる。
- カード: 既存の `plan_load` に任意の `estimate`（`runId`、方式の版、テンプレートとの差の要約、終了日: テンプレート / P50 / P80、負荷の最大: 前 / 後、データの量の 1 行）。**項目ごとの根拠は `plan_estimate_run` に置く**（55,000 バイトの上限）。「根拠を見る」から T_i・D50・D80・n・α・比・出典・規則の ID と引用・MR セットのリードタイムとリポジトリの内訳を開く。
- 承認: `/plan/load` は `estimateRunId` を受け、その時点の稼働量で平準化をやり直す。**終了日か担当が変わっていれば** 409 `PLAN_ESTIMATE_STALE`（黙って違う計画を書かない）。書き込みで `templateKey`・`scheduleLoadId`・基準・`ownerId`・`durationDays = D50` を保存する。
- 入口: Web の全体計画の「計画を作る」と、MCP の新しい道具 `propose_plan_from_actuals`（スコープ `backlog:write`。応答は `runId` と要約だけで、**人ごとの負荷は返さない**）。

## 4. 安全と個人の情報

| 項目 | 決まり |
|---|---|
| 機密の PBI（D-5） | すべて機密の葉は学習から外す。一部が機密ならその PBI の時間と MR を入れない。機密の教訓は規則の下書きに入れない。MCP と LLM に渡さない |
| 他のプロジェクトの実績 | 名前を出すのは `project.view` を持つときだけ。見えないものは「他のプロジェクト n 件」とまとめ、**見えない出典が 3 件未満なら使わない**（Q3） |
| リポジトリの内訳 | リポジトリの名前は `project.view` を持つときだけ。他のプロジェクトの出典では「リポジトリ n 本」とだけ出す。GitStation が読ませない MR はもともと届かない（I1 の `prove_visible`） |
| 他の人の負荷 | 新しい操作 `plan.viewMemberLoad`: 本人、プロジェクトの PO / SM、課長。見せるのは使用率と重なる日だけで、**他のプロジェクトの項目の名前は出さない**（Q2） |
| 新しい操作（`policy.ts`） | `plan.estimate`（PO / SM）、`plan.actualsFinalize`（PO / SM）、`plan.lessonRule.confirm`（プロジェクトの規則は PO / SM、系統・種類の規則は管理者か系統の持ち主。Q6）、`plan.viewMemberLoad`、`capacity.manage`（本人と PO / SM、課長）。どれも allow と deny の行を `policy.test.ts` に足す |
| LLM | 社内の Ollama だけ。入力のハッシュ・モデル・プロンプトの版を `plan_estimate_run` に残す |
| 保持 | 担当の ID の保持期間と退職者の扱いは Q8 |

## 5. 評価（既定で見せる前に測る）

- **バックテスト**（E2、`scripts/plan-backtest.ts`）: 終わったプロジェクトを `closedAt` の順に並べ、各プロジェクトについて開始日より前に終わったものだけで見積もり、実績と比べる。
- **指標**: 葉の期間の MAPE（日数で重み）とテンプレートだけの MAPE との比、P80 の当たりの率、終了日の誤差、平準化の効果（1.0 を超えた人日の減り）、リポジトリの数で層別した MAPE。
- **既定で見せる条件（系統ごと、Q9）**: 段 1 で照合できる終わったプロジェクトが 5 件以上、MAPE がテンプレートだけより 15% 以上小さい、P80 の当たりが 70〜90%、終了日の誤差が悪くない。満たさない系統は「実績で調整」を既定で切り、「この種類の実績はまだ n 件です。テンプレートどおりに作りました」と出す。平準化は条件に関係なく使える。
- **継続**: 実績を確定するたびにバックテストを実行して記録し、悪くなったら PO / SM に通知する。
- **利用者の反応**: 項目ごとの「高すぎる / 低すぎる / この反省は当てはまらない」と、承認から 14 日以内に PM が直した日数と担当（暗黙の反応）。方式を変えるときはバックテストで比べてから（01 §6.5 と同じ）。

## 6. MR の分け方

- 置き場所: **[KF]** = Kanflow のリポジトリ（worktree `D:\APP\Kanflow-v2`。本番の `D:\APP\Kanflow` には触れない）、印なし = GitStation。
- 各 MR の終わり: Kanflow は lint・typecheck・test（PG の DB のテストを含む）。GitStation は fmt・clippy・nextest、`gen-catalog` / `gen-authz` の差分なし。`/review` で CRITICAL / HIGH が無いこと。
- **時期**: E0 は**今すぐ**（M3 と並行。2026-11-02 着手）。E1〜E5 は Kanflow の K3 の後（M3 後半〜M4）。E6（LLM）は M5 で、**L-5（衝突とブロックの解消案、2026-11-02 に M5 へ前倒し）と同じ段**なので、共有の Ollama の同時実行の枠を分ける（E6 は夜間のまとめ処理と提案の時の対話、L-5 は MR の画面から）。G1 は I1 の後。

| MR | 中身 | 先に書くテスト | 評価役 | 時期 |
|---|---|---|---|---|
| **E0 [KF]** `feat/L4-plan-template-key-baseline` | `PlanItem.templateKey`・`scheduleLoadId`、表 `plan_schedule_load`、`BaseScheduleItem` の任意の `role`・`load`・`formerKeys`、`/plan/load` で `key`・読み込みの記録・基準を保存、既存の読み込みの埋め戻しのスクリプト（乾式の実行つき） | `load_stores_template_key_on_every_row`、`load_sets_baseline_equal_to_planned`、`load_records_source_ref_and_hash`、`old_schedule_without_role_and_load_still_loads`、`former_keys_map_to_current_key`、`backfill_leaves_ambiguous_rows_null` | `typescript-reviewer`、`database-reviewer`、`pr-test-analyzer` | **今** |
| **E1 [KF]** `feat/L4-plan-actuals-snapshot` | §2.2 と §2.4 の表、「実績を確定」、確定の前の検査、純粋な関数（日数・滑り・待ち・同時に持つ数・負荷・MR セットのリードタイム）、機密の除外、`project_attribute`、`GitLink` の MR の項目 | `snapshot_counts_workdays_when_skip_weekends`、`incomplete_leaf_is_kept_but_not_learnable`、`confidential_pbi_hours_are_excluded`、`refinalize_supersedes_without_update`、`concurrent_load_stores_no_other_project_names`、`on_hold_days_from_activity_log`、`mr_set_lead_time_spans_all_bound_repositories`、`closed_unmerged_mr_not_in_set`、`per_repo_breakdown_sums_to_item_totals`、`unreadable_repository_gives_null_not_zero`、`same_pbi_in_two_repos_counted_once_as_set` | `typescript-reviewer`、`database-reviewer`、`security-reviewer`、`silent-failure-hunter` | K3 の後 |
| **E2 [KF]** `feat/L4-plan-estimate-engine` | `lib/plan-estimate/`（照合・重み・外れ値・縮め・P50 / P80・待ち・全体のバッファ）、バックテスト（リポジトリの数の層別を含む）と `plan_estimate_eval` | 性質のテスト: `no_observations_returns_template`、`estimate_moves_toward_data_as_n_grows`、`outlier_beyond_3_mad_is_dropped_and_reported`、`p80_never_below_p50`、`ratio_clamped_to_bounds`、`future_projects_never_used`、`same_input_same_output`、`backtest_reports_mape_by_repo_count` | `typescript-reviewer`、`pr-test-analyzer` | E1 の後 |
| **E3 [KF]** `feat/L4-plan-lesson-rules` | `plan_lesson_rule`、6 種類の適用と検証、確定の流れと画面 | `draft_rule_never_changes_plan`、`rule_creating_cycle_is_rejected_with_reason`、`added_items_capped`、`scale_product_bounded`、`rules_apply_in_fixed_order`、権限の行 | `typescript-reviewer`、`database-reviewer`、`security-reviewer` | E2 と並行 |
| **E4 [KF]** `feat/L4-plan-capacity-leveling` | `member_capacity`、m_u の学習、§3.5 の平準化、`scheduleFrom` の `notBefore`、`plan.viewMemberLoad`・`capacity.manage` | `critical_path_item_scheduled_first`、`no_user_exceeds_capacity_after_leveling`、`zero_load_item_consumes_no_capacity`、`split_suggested_only_when_it_finishes_earlier`、`unassigned_role_leaves_owner_null`、`not_before_respected_by_server_recompute`、`member_cannot_see_others_load`、`po_sees_utilization_but_not_other_project_names`、500 件で 2 秒以内 | `typescript-reviewer`、`security-reviewer`、`database-reviewer`、`pr-test-analyzer` | E2 の後 |
| **E5 [KF]** `feat/L4-plan-propose-from-actuals` | `plan_estimate_run`、Web の「計画を作る」、MCP `propose_plan_from_actuals`、カードと根拠の画面、`/plan/load` の再検査と `PLAN_ESTIMATE_STALE`、反応の記録、系統ごとの既定の切り替え | `card_fits_size_limit_with_500_items`、`approval_recomputes_and_refuses_stale_plan`、`mcp_response_has_no_per_person_load`、`invisible_sources_under_3_are_not_used`、`repo_names_hidden_for_invisible_source_projects`、`proposal_contains_only_template_or_rule_items`、`same_proposal_loads_once`、`gate_off_lineage_shows_template_with_reason` | `typescript-reviewer`、`security-reviewer`、`a11y-architect`、`i18n-sync-auditor`、`silent-failure-hunter` | E3・E4 の後 |
| **E6 [KF]** `feat/L4-plan-estimate-llm` | 規則の下書き（引用つき）、段 3・4 の候補の採否、説明の文。止まったら決定的な部分に戻る | `llm_timeout_falls_back_and_is_logged`、`invalid_llm_output_retried_once_then_dropped`、`llm_cannot_emit_numbers_outside_rule_schema`、`injected_retro_text_does_not_change_rules`、`confidential_lessons_not_in_prompt`、規則の下書きの評価セット | `llm-integration-reviewer`（M5 で追加の予定）、`security-reviewer`、`typescript-reviewer` | **M5**（L-5 と同じ段） |
| G1 `feat/cross-dev-status-mr-timing` | 開発状況 API の MR に `opened_at`・`merged_at`・`closed_at`・`version_count`・`diff_stats` を足す（足すだけ）。結んだ全リポジトリの MR を返す。イベントの履歴の `merge_request.opened` / `merged` が時刻を持つか確かめる（**未確認**。無ければ足す） | `dev_status_reports_opened_and_merged_at`、`version_count_matches_merge_request_versions`、`diff_stats_absent_when_not_computed_is_null_not_zero`、`dev_status_returns_mrs_from_every_bound_repository`、`mr_in_repository_caller_cannot_read_is_omitted` | `rust-reviewer`、`authz-matrix-auditor`、`database-reviewer`、`pr-test-analyzer`、`error-contract-auditor` | I1 の後 |

**エラーコード**: GitStation の G1 は既存のコードで足りる見込み。Kanflow は `PLAN_ACTUALS_INCOMPLETE`（400）、`PLAN_RULE_INVALID`（400）、`PLAN_ESTIMATE_STALE`（409）、`PLAN_ESTIMATE_RUN_NOT_FOUND`（404）。LLM の失敗は利用者に見せず、記録して元の表示に戻す（07 W14）。

**運用**: メトリクス（見積もりの実行の時間、LLM の時間切れと戻り、`PLAN_ESTIMATE_STALE` の回数、確定の時に読めなかったリポジトリの数）、ログ（`runId`・入力のハッシュ・方式の版）。「見積もりが変なとき」の調べ方（`runId` → 根拠 → 出典 → リポジトリの内訳 → バックテストの記録）を Kanflow の運用の文書に足す。

## 7. 危険と対策

| 危険 | 対策 |
|---|---|
| データが貯まらない | E0 を今入れる。埋め戻しのスクリプト |
| 実績の日付の入力が雑 | 確定の前の検査、`complete = false`、MAD の外れ値、縮め、バックテストで既定を切る |
| 過去の悪い進め方を学ぶ | 比の上限 3.0、P80 を別に見せる、反省の規則、`includeInLearning` を外せる |
| リポジトリを分けた・まとめたことで MR の数字が別の作業に見える | MR セットを単位にする。リポジトリの数は層別で確かめてから照合に入れる |
| 確定の時に結び付きが外れていて MR が欠ける | 欠けは null と一覧で示す |
| 他の人の負荷が監視の道具になる | 使用率と重なる日だけ、見る人を限る（Q2） |
| 他のプロジェクトの数字が漏れる | `project.view`、見えない出典は 3 件以上でまとめる、リポジトリの名前を隠す |
| LLM の規則が計画を壊す | 下書きは効かない、確定は人、6 種類だけ、検証で外す |
| M5 の LLM が L-5 と取り合う | 同時実行の枠を分け、E6 のまとめ処理は夜間 |
| カードの日付と書き込みの日付が違う | 制約として渡しサーバーで計算し直す。違えば 409 |
| 範囲が広がる | E0 → E2 → E4 の順に、それぞれ単独で価値がある形で出す |

## 8. 完了の条件

1. E0: 新しく読み込んだ全体計画の葉がすべて `templateKey` と基準を持つ（DB のテスト）。
2. E1: 合成の 1 プロジェクト・リポジトリ 3 本・PBI 1 件に MR 3 本で、MR セットのリードタイムが 3 本をまたいで正しく出て、内訳の合計が葉の値と一致する。
3. E2: 合成データ（系統 2 つ・終わったプロジェクト 12 件・意図した外れ値）のバックテストで、MAPE がテンプレートだけより小さく、P80 の当たりが 70〜90%。
4. E4: 合成の 3 人・2 プロジェクトの掛け持ちで、平準化の後に 1.0 を超える人日が 0、終了日の延びはクリティカルパスの必要な分だけ。
5. E5: Kanflow v2 の試験用インスタンスで、テンプレート → 実績の調整 → 承認 → 全体計画を Playwright で通す。データの無い系統はテンプレートどおり。
6. 本物のデータで既定を有効にするのは、§5 の条件を系統ごとに満たしてから（利用者の承認）。

## 9. 利用者に決めてほしいこと（推奨つき）

1. **稼働量の決め方**。推奨: 人ごとに FTE（既定 1.0）と同時に持つ数の上限（既定 2、実績から学ぶ）。1 日 7.5 時間。休みと会社の休日の正本（社内のカレンダー）はあるか。無ければ当面は土日だけを飛ばす。
2. **他の人の負荷を誰が見られるか**。推奨: 本人、そのプロジェクトの PO / SM（使用率と重なる日だけ）、課長。稼働量を入れるのは本人と PO / SM・課長。
3. **見えないプロジェクトの実績を使ってよいか**。推奨: 使う。名前を出さず 3 件以上をまとめたときだけ。部署をまたいでよいか（課の範囲に限るか）。
4. **全体のバッファを計画の行として入れるか**。推奨: 入れない。P50 と P80 の終了日を並べる。約束の日付は P80 に合わせる運用を勧める。
5. **設備・工程のマスタの正本**（IoT の PFS の DB か、別の台帳か）。決まるまでは自由な文字列。
6. **反省の規則を確定できる人**。推奨: プロジェクトの規則は PO / SM。系統・種類の規則は管理者かテンプレートの持ち主。
7. **既定の数値**: K = 3、半減期 365 日、比 0.5〜3.0、外れ値 3 MAD。推奨: この値で始め、バックテストで見直す。
8. **保持と退職者**。推奨: プロジェクトと同じ期間残し、退職した人は役割の名前に置き換える。
9. **AI の案を既定で見せる条件**。推奨: 系統ごとに、終わったプロジェクト 5 件以上・MAPE 15% 以上の改善・P80 の当たり 70〜90%。平準化は最初から使える。
10. **複数のリポジトリの MR の数え方**。推奨: PBI ごとの MR セットを単位に、「最初に開いた日から最後のマージまで」。マージしないで閉じた MR は数えない。
11. **仕様の改訂（`spec-drift`）を実績に残すか**。推奨: 残すが、見積もりには使わず根拠に出すだけ。
12. **時期**。推奨: E0 は今すぐ、E1〜E5 は M3 後半〜M4、E6 は M5（L-5 と同じ段）。G1 は I1 の後。
