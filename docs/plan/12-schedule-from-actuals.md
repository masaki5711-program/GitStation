# 12 実績から作るスケジュール — ベースの計画に、過去の実績と反省点を落とし込み、負荷を配り直す

作成: 2026-11-02（planner）。状態: **計画に盛り込み済み（2026-11-02、利用者の指示「ロジックも含めて作って計画に盛り込んでください」）**。§9 の答えは各 MR の着手前に聞く。E0 は 2026-11-02 に着手。**同じ日にセキュリティの設計レビューを受け、決めたことを各節に反映した（一覧は §10。再レビューの分も同じ表）。**
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
| 12 | 権限: `plan.write` は PO / SM。判定は `policy.ts` の 1 か所。操作は**すべてプロジェクトごと**（`can(actor, action)`）で、許す相手は `po`・`sm`・`member`（明示のメンバー）・`section_manager`（プロジェクトが属する課の課長）の 4 種類。システムの管理者はすべて許される。**人や系統の単位の操作は今は無い** | `policy.ts` 9〜23 行・99〜164 行 |
| 13 | `GitLink` は MR の `state` と `mergedAt` を持つ。開いた日時・差分の大きさ・版の数は持たない | `schema.prisma` 1420〜1461 行 |
| 14 | 同じリポジトリの計画テンプレート（`PlanTemplate`）の段は `key` を任意で持つ。`PlanTemplateApplication.content` は読み込んだ時点の内容を保存する | `plan-templates/schema.ts` 63 行、`schema.prisma` 180〜203 行 |
| 15 | **1 つのプロジェクトに複数のリポジトリを結べる。** `Project.gitRepositories` がある。`GitLink` には `repositoryId` があり、一意の鍵は `(repositoryId, kind, ref, backlogItemId)` なので、1 つの PBI が複数のリポジトリの MR を持てる。読む先（`GitIntegration`）はプロジェクトに 1 つ | `schema.prisma` 330〜332 行・1296 行・1318 行〜・1453〜1457 行 |

GitStation 側: 開発状況 API（02 §5.2）は M3 の I1 で作る。応答の MR には `project` の欄があり、リポジトリをまたいで返す。`/metrics/flow`（F-14）は第 2 段（M4）。F-17 と F-33 は Could で第 3 段。リポジトリをまたぐ連携 (a)〜(c) は M3 後半（11 §5.7 の X1〜X3）。MR セットのマージの順序（C-3）は M4。

### 0.4 前提の点検（提案された形は合っているか）

1. **「AI が生成する」をそのまま作ると目的に反する。** LLM が過去の計画を読んで新しい計画を書くと、写しに近くなり、数字の根拠も追えず、試験もできない。01 §3 の原則 3（決定的な処理が主、LLM は補助）と 07 W14 に合わせ、**期間と負荷の数字はコードで決め、LLM は反省点の文を規則の下書きにする役と説明の文に限る**。LLM が無くても機能の全体が動く（§5）。
2. **負荷の配り直しは、過去のデータが無くても初日から役に立つ。** テンプレートの期間のままでも、人の掛け持ちが重なれば平準化できる。データを貯める部分（E0）と平準化（E4）を、LLM（M5）より先に出す価値がある。
3. **今のままではデータが貯まらない。** `key` と基準が保存されていない（§0.3 の 1・3）。**E0（`key` と基準の保存）は今すぐ入れる**のが最も効く。
4. **計画の持ち主は Kanflow。** 計算と保存は Kanflow で行う。GitStation が出すのは、ソフトウェアのプロジェクトの MR の事実（リードタイム・規模・手戻り）だけで、必須ではない（設備・工程には MR が無い）。**MR の事実は「PBI ごとの MR セット」を単位に、プロジェクトに結んだ全リポジトリをまたいで集計する（§2.4）。**
5. **費用**: 新しい表が 11（§2.1）、MR が Kanflow で 7 本・GitStation で 1 本。攻撃面は「他のプロジェクトの数字が見える」こと・「他の人の負荷が見える」こと・「LLM と根拠の画面から見えないはずの文が漏れる」こと（§4）。

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
| 葉ごとの PBI の内訳（使う時の機密の再確認用） | 無い | 表 `plan_actuals_item_pbi`（`snapshotItemId`、`backlogItemId`、PBI ごとの時間・MR セットの値。§2.2、E1） |
| リポジトリごとの MR の内訳 | `GitLink`（時刻と規模は無い） | 表 `plan_actuals_item_repo`（§2.4、E1）。PBI ごとに 1 行 |
| 反省点 → 計画の変更 | 文だけ | 表 `plan_lesson_rule`（§2.3、E3） |
| 人の稼働量 | **無い** | 表 `member_capacity`（`userId`、`projectId`（null = 人の単位の FTE、値あり = そのプロジェクトへの割り当て）、`from`、`to`、`fte` 0〜1）。無ければ既定の 1.0。**自由記述の `note` は持たない**（§4）（E4） |
| 見積もりの実行と根拠 | 無い | 表 `plan_estimate_run`（`projectId`・`createdById`・入力のハッシュ・`expiresAt`・方式の版・項目ごとの根拠（出典は ID で持つ）・平準化の結果・LLM のモデルとプロンプトの版）（E5） |
| 利用者の反応 | 無い | 表 `plan_estimate_feedback`（`runId`、`key`、`kind` = `too_high`・`too_low`・`rule_not_applicable`・`other`、`note`（500 字まで）、`userId`）（E5） |
| バックテストの記録 | 無い | 表 `plan_estimate_eval`（系統・評価の日・n・MAPE・P80 の当たりの率・終了日の誤差・方式の版）（E2） |
| 設備・工程のマスタの ID | **無い** | 表 `project_attribute`（`projectId`、`kind` = `equipment`・`process`・`tag`、`ref`）。正本が決まるまでは自由な文字列（Q5）（E1） |
| MR の事実（ソフトウェアだけ） | `mergedAt` だけ | GitStation の G1 で開発状況 API に項目を足す。Kanflow は `GitLink` に `openedAt`・`versionCount`・`additions`・`deletions` を足す（E1） |

### 2.2 実績のスナップショット（E1）

**作る時**: PO / SM が「実績を確定」を押した時（新しい操作 `plan.actualsFinalize`）。プロジェクトを完了にした時は催促を 1 回出す。確定し直しは新しい行を足し、古い行に `supersededAt` を入れる（行を書き換えない）。

**理由**: 計画は後から直される・ゴミ箱に入る・リポジトリの結び付きが外れる。バックテストを同じ結果で繰り返すには、その時点の値を凍結する。他のプロジェクトの負荷も、集計した数字だけを残せる。

**凍結しないもの — 機密の印（設計レビューで決定）**: 数字は凍結するが、**機密かどうかは凍結しない。** PBI と `Lesson` の `confidential` は確定の後にも付け外しされるので、スナップショットには**PBI の ID を葉ごとに残し**（`plan_actuals_item_pbi`）、学習・引用・根拠の表示の**使う時ごとに今の `BacklogItem.confidential` を結合して確かめる**。PBI が消された・ゴミ箱から完全に削除された・読めない場合は機密として扱う（安全な側に倒す）。印が変わってもスナップショットを作り直す必要はなく、行は書き換えない。

`plan_actuals_snapshot`: `projectId`、`projectType`、`lineage`、`scheduleHash`、`skipWeekends`、`closedAt`、`finalizedAt`、`repoCount`、`includeInLearning`（既定 true。外す・戻すのは `plan.actualsFinalize` を持つ人（PO / SM）だけ。理由と操作した人を `ActivityLog` に残す）、`version`、`supersededAt`。

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
| `ownerId` | 担当。同時に持つ数（m_u）の学習だけに使い、**根拠の画面には出さない**（役割の名前だけを出す）。保持と仮名化は §4 |
| `ownerConcurrentAvg` / `ownerConcurrentPeak` | 期間中に担当が同時に持っていた葉の数（全プロジェクト）。**他のプロジェクトの名前は残さない** |
| `effortHours` / `estimatedHours` | 結んだ PBI のタスクの実績・見積の時間（ソフトウェア）。**確定の時点では PBI ごとに `plan_actuals_item_pbi` に残し、葉の値は使う時に「今機密でない PBI」の分だけを合計する** |
| `loadObserved` | effortHours ÷ (actualDays × 1 日の時間（既定 7.5、Q1））。0〜1.5 に収める |
| `reworkCount` | PBI の再オープン + **全リポジトリの** MR の再オープンと revert |
| `mrSetCount`、`mrCount`、`repoTouched` | MR セットの数、MR の数（全リポジトリ）、触ったリポジトリの数 |
| `mrSetLeadTimeP50Hours`、`mrLeadTimeP50Hours` | MR セットと MR 1 本ずつのリードタイムの中央値（無ければ null） |
| `mergeOrderViolations` | C-3 の順序の警告の数（C-3 の後。それまでは null） |
| （`confidential` の列は持たない） | 使う時に計算する: 結んだ PBI が**今**すべて機密なら、その葉は学習の対象外。一部が機密ならその PBI の時間・MR の値を除いて合計する。PBI を結んでいない手動の葉は機密ではない |

`plan_actuals_item_pbi`（葉 × PBI）: `snapshotItemId`、`backlogItemId`、`effortHours`、`estimatedHours`、`reworkCount`、`mrSetLeadTimeHours`、`mrCount`。上の表の PBI に由来する列（`effortHours`・`estimatedHours`・`loadObserved`・`reworkCount` の PBI の分・`mrSetCount`〜`mrLeadTimeP50Hours`）は**葉の行に保存せず**、この表と `plan_actuals_item_repo` から使う時に今の `confidential` を結合して計算する（1 つの関数 `learnableItemFacts(snapshotItemIds, now)` だけを通す。機密の再確認を飛ばす経路を作らない）。

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

表 `plan_lesson_rule`: `kind`、`params`（zod で検証）、範囲（`project`（出典のプロジェクトだけ）・`lineage`・`projectType`・`project_attribute` の ref のどれか 1 つ以上）、`state` = `draft` → `confirmed` → `retired`、出典（`sourceProjectId`、`lessonId?`、`backlogItemIds?`、`sprintId?`、引用した文）、`createdBy` = `human` / `llm`、`promotedById?`・`promotedAt?`、`confirmedById`、`confirmedAt`。**計画に効くのは `confirmed` だけ。** W11 の `Lesson` は開発の失敗の署名が鍵なので使い回さない。

**範囲と機密（設計レビューで決定）**:
- **機密は凍結しない**（§2.2 と同じ）。規則は出典の `lessonId`・`backlogItemIds` を持ち、使う時に今の `confidential` を結合して確かめる。出典に今機密のものが 1 つでもあれば、その規則は**出典のプロジェクトの中でだけ**効き、引用は出典のプロジェクトの外では出さない。系統・種類の範囲の規則で出典が後から機密になったものは、効かせず「出典が機密になったため停止中」と管理者に示す。
- **スプリントの振り返り（`retrospectiveKeep/Problem/Try`）には機密の印が無い**ので、振り返りの文は**プロジェクトの内部の情報**として扱う。振り返りから作った規則（人・LLM のどちらでも）は `project` の範囲だけ。
- **系統・種類の範囲へ広げる（昇格）** には、出典のプロジェクトの PO が昇格を申請し、管理者が確定する（§4 の `plan.lessonRule.confirm`）。**昇格のときに引用した文を消し**、出典は「プロジェクト 1 件」とだけ出す（`sourceProjectId` は監査用に残すが、出典のプロジェクトを `project.view` できない人には出さない）。
- 確定は**1 件ずつ**、種類と数字（`factor`・`days`・`durationDays`・`parts`・`max`）を画面に出した状態で行う。まとめて確定する操作は作らない。

### 2.4 複数のリポジトリを結んだプロジェクト（MR の事実の集計）

- **単位は PBI ごとの MR セット**（02 F-24、04 C-3）: 同じ PBI を参照する、結んだ全リポジトリの MR。マージしないで閉じた MR は数えない。
- **MR セットのリードタイム** = セットで最後にマージされた `merged_at` − 最初に開いた `opened_at`。PBI がリポジトリの数だけ分かれても 1 つの作業の時間として学べる。参考に MR 1 本ずつの中央値も残す。
- **表 `plan_actuals_item_repo`**: `snapshotItemId`、`backlogItemId`、`repositoryRef`（GitStation のプロジェクトのパスと表示名）、`mrCount`、`leadTimeP50Hours`、`additions`、`deletions`、`versionCount`、`reworkCount`。葉の値は、使う時に今機密でない PBI の行だけの合計か中央値（§2.2）。リポジトリの名前と差分の大きさを誰に見せるかは §4。
- **リポジトリの数の違い**は葉の期間の比に直接入れない。`repoTouched` を根拠に出し、E2 のバックテストで「数で分けたほうが当たる」と分かったら照合の似ている度に係数を足す（方式の版を上げる）。
- **取り込み**: Kanflow の GitStationSource（11 K3）は結んだ全リポジトリのイベントを 1 つのカーソルで読む。スナップショットは `GitLink` から作り、確定の時点の値で凍結する。開発状況 API を呼ぶのは欠けを埋めるときだけ。
- **仕様の版**: `sourceRef` にベースのリポジトリの固定した版（X1）を写す。`spec-drift`（X2）で仕様が改訂されたプロジェクトは `specRevisedDuringProject` を残し、見積もりには使わず根拠に出す（Q11）。

## 3. 設計 — ロジック（決定的なものが先、LLM は後）

記号: 葉 i の `key` を k_i、テンプレートの日数を T_i、役割を r_i、負荷を ℓ_i。過去の観測 j の予定の日数を P_j、実績を A_j。対象は葉だけで、マイルストーンは 1 日に固定する。

### 3.1 照合（過去の観測を集める）

対象は `includeInLearning = true` かつ `complete = true` かつ**使う時の機密の確認（§2.2 の `learnableItemFacts`）で機密でない**もので、**`closedAt` と `finalizedAt` が基準日 R より前**のもの（先の情報を漏らさない）。**基準日 R は提案する人が選ぶ開始日ではなく、実行を作った日をその月の 1 日に切り捨てた日**（再レビューで決定）。開始日を 1 日ずつ動かして実行し直しても照合の集合と重みが変わらないので、境目の 1 件の見えない出典を割り出せない。バックテスト（§5）だけは、評価するプロジェクトの開始日の月の 1 日を R にする（内部の評価で、画面に出さない）。出典は、提案する人が `project.view` を持つ「見える出典」と、持たない「見えない出典」に分け、見えない出典は §3.2 の決まりでだけ使う。

| 段 | 条件 | 似ている度 s_j |
|---|---|---|
| 1 | 同じ `lineage` で同じ `key`（または `formerKeys`） | 1.0 |
| 2 | 系統は違うが同じ `projectType` で同じ `key` | 0.7 |
| 3 | 同じ `projectType` で `project_attribute` の ref が 1 つ以上同じで、名前が似ている（`pg_trgm` similarity ≥ 0.6） | 0.5 × similarity |
| 4 | 同じ `projectType` で名前が似ている（≥ 0.75） | 0.3 × similarity |

段 1 の観測が 3 件以上あれば段 3・4 は使わない。段 3・4 の採否は E6（M5）から LLM が補助する（返すのは候補の ID の部分集合だけで、似ている度は式のまま）。**LLM に渡す候補は見える出典のものだけ**で、見えない出典の候補（項目の名前もプロジェクトの内部の情報）は式だけで採否を決める（§3.4）。

重み: w_j = s_j × 0.5^(経過日数_j / H)。経過日数_j = R − `closedAt`_j（基準日 R から数える。開始日からは数えない）。H は半減期（既定 365 日、Q7）。

### 3.2 期間の見積もり（P50 / P80）

対数の比 x_j = ln(A_j / P_j) を使う（右に裾が長い分布に頑健。比を今の T_i に掛けるので、テンプレートの版の違いにも影響されない）。

```
estimate(i):
  obs = match(i)                                    # §3.1。基準日 R で切る
  V, H = obs を見える出典と見えない出典に分ける
  hid = hidden_pseudo_obs(H)                        # 下の「見えない出典の扱い」。使えなければ無し
  if V is empty and hid is none: return prior(i)    # §3.3
  m   = weighted_median(x[V], w[V])                 # V が空なら m = hid.x
  mad = max(weighted_median(|x[V] - m|, w[V]), 0.05)
  kept = V where |x_j - m| <= 3 * 1.4826 * mad      # 外れ値は見える出典だけで除く（見えない出典は刈り込み済み）
  pool = kept ∪ {hid}                               # hid は 1 つの擬似の観測（x = hid.x、w = hid.w）
  m*  = weighted_median(x[pool], w[pool])
  n   = (Σw)^2 / Σw^2   over kept + (hid.h' - 2 if hid)   # 有効な件数
  μ0  = prior_logratio(projectType)                 # §3.3
  α   = n / (n + K)                                 # K = 3（Q7）
  x̂   = α * m* + (1 - α) * μ0                       # テンプレート側へ縮める
  s80 = weighted_quantile(x[pool], w[pool], 0.8) - m*   (n >= 3 のときだけ。それ以外は S0)
  ŝ   = α * max(s80, 0) + (1 - α) * S0              # S0 = 0.25（約 +28%）
  r50 = clamp(exp(x̂), RMIN, RMAX)                   # 0.5〜3.0
  r80 = clamp(exp(x̂ + ŝ), RMIN, RMAX)
  if 見えない出典が効いた葉（hid または μ0 の見えない分を使った）:
    r50, r80 = round(r50, 0.1), round(r80, 0.1)     # 日数は必ず丸めた比から計算する（正確な比からは計算しない）
  D50 = clamp(ceil(T_i * r50), 1, 3650)
  D80 = clamp(ceil(T_i * r80), D50, 3650)
  lag = shrink(weighted_median(waitGapDays), toward 0, K)   # 1 日以上のときだけ「待ち」を提案。見えない出典の分は同じ規則（下の 6）
  return { D50, D80, n, α, r50, kept, dropped, sources, lag,
           mrSet: { leadTimeP50, repoTouchedP50 } }  # ソフトウェアだけ。根拠の表示用
```

- **プロジェクト全体のバッファ**（各葉に余裕を足さない）: クリティカルパス上の葉について B = ceil( sqrt( Σ (D80_i − D50_i)^2 ) )。カードに「P50 の終了日」と「P80 の終了日 = P50 + B」を並べる。行として入れるかは Q4。
- 負荷の学習（ソフトウェア）: `loadObserved` の重みつき中央値を ℓ_i の方向に同じ α で縮めて ℓ̂_i とする。
- MR セットのリードタイムは期間の見積もりには使わず、F-33 の参考情報と根拠の表示に使う。

**見えない出典の扱い（設計レビューで決定。Q3）**:
1. **葉ごとに数える。** 葉 i の観測のうち見えない出典（プロジェクトの単位で数える）が 3 件未満なら、その葉では見えない出典を**使わない**（見える出典だけで見積もる）。プロジェクト全体で 3 件あっても、葉ごとに足りなければ使わない。
2. **差分で 1 件を割り出させない — 2 件ずつ進める。** 葉 i で使う見えない出典は、`finalizedAt` の古い順に並べた最初の h' 件だけにする。h' = 見えない出典の数 h を**偶数に切り下げた数**（h = 5 なら 4）。新しく確定した見えない出典 1 件だけでは見積もりが変わらず、変わるのは 2 件がそろった時だけなので、前後の実行の差から 1 件のプロジェクトの比を割り出せない。1 と合わせると、見えない出典が使われるのは葉ごとに 4 件以上のとき。確定し直し（`supersededAt`）は同じ出典の入れ替えとして扱い、数を増やさない。
3. **丸めて出す。** 根拠に出す比（r50、P80 の比、見えない出典のまとめの比）は **0.1 単位に丸めた値**だけ。件数 n と α は「〜3 件」「4〜9 件」「10 件以上」の幅で出す。**見えない出典が効いた葉の D50 / D80 は、0.1 単位に丸めた比から計算した日数だけ**を使う（再レビューで決定）。正確な比から計算した日数は、カード・根拠・書き込む `durationDays`・平準化・全体のバッファ B のどこにも使わない（日数 ÷ T_i から正確な比を逆算させない）。
3-2. **見えない出典は 1 つの値にまとめる — 刈り込み平均（再レビューで決定）。** 中央値は 1 件の出典の値そのものになりうるので、見えない出典の分には使わない。`hidden_pseudo_obs(H)`:
   - 見えない出典をプロジェクトごとに 1 つの値にする: y_p = そのプロジェクトの葉 i の観測の x の平均。
   - 1・2 の決まりで残った h' 件（4 件以上の偶数）の y を並べ、**最大と最小を 1 件ずつ除いた平均** x_H = (Σ y_p − max y − min y) / (h' − 2) を擬似の観測の値にする。重みは w_H = (h' − 2) × 見えない出典の w の平均。重みは基準日 R から計算するので、開始日では変わらない。
   - 擬似の観測は外れ値の除去に加えず（すでに刈り込み済み）、見える出典の重みつき中央値に 1 件として加わる。見える出典は中央値のまま。

4. **出典ごとの行を出さない。** 見えない出典は「他のプロジェクト n 件（幅）」の 1 行にまとめ、**プロジェクトごとの比・MR セットのリードタイム・n・除いた / 残した（kept / dropped）・リポジトリの数を出さない。** 外れ値として除いた件数も、見えない出典の分は見える出典の分と合わせた数でだけ出す。
5. 根拠は `plan_estimate_run` に**出典の ID で**持ち、名前は**読む人の権限でその都度**解決する（作った人に見えて読む人に見えない出典は、読む人には見えない出典として表示する）。読む人にとって葉の見えない出典が 4 件未満になる場合は、まとめの行の比も出さず「他のプロジェクトの実績を含む」とだけ出す。
6. **見えない出典を含みうる集計はすべて同じ決まり**（再レビューで決定）。葉の比だけでなく、種類の傾向 μ0（§3.3）、待ち `lag`、負荷 ℓ̂、同時に持つ数の上限 m_u（§3.5）、§5 の既定の切り替えに出す件数も、見えない出典の分は 1（集計の単位ごとに 3 件以上）・2（偶数に切り下げ）・3-2（刈り込み平均）・4（出典ごとの行を出さない）を当て、見える出典の分と合わせる。件数は幅で出す。照合と重みは基準日 R で決める（§3.1）。

### 3.3 データが無い・少ないとき

| 状態 | 動き |
|---|---|
| 観測が無く、種類の観測も無い | μ0 = 0。D50 = T_i（**テンプレートどおり**）。D80 = T_i × e^0.25 |
| 項目の観測は無いが、同じ種類の観測は N 件ある | μ0 = (N/(N+10)) × 種類の傾向。種類の傾向 = 見える出典の観測と、見えない出典の擬似の観測 1 件（種類の単位で §3.2 の 1・2・3-2 を当てる。見えない出典のプロジェクトが 3 件未満なら使わない）の重みつき中央値。N は見える出典の件数 + (h' − 2)。印「種類の傾向だけ」。μ0 に見えない分が入った葉は「見えない出典が効いた葉」として比を丸める（§3.2） |
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
1. 実績の確定の時、**そのプロジェクトの**スプリントの振り返りの文（Problem / Try）、`project:<id>` の確定した教訓（**今機密でないものだけ**）、確定の時のメモを読み、§2.3 の形の `draft` の規則を出す。各規則に**引用した文と出典の ID**を付ける。zod で検証し、不正なら 1 回だけ再試行し、それでも不正なら捨てて記録する。PO / SM が確定するまで効かない。
2. 提案の時、§3.1 の段 3・4 の候補の採否を補助し、根拠の説明の文（ja / en / id）を書く。
3. 入力はすべてデータとして扱い（振り返りの文にプロンプトインジェクションが入りうる）、道具は渡さない（01 §6.1）。

**LLM に渡すもの（設計レビューで決定）**:
- **頼んだ人が見られるものだけ。** 確定の時は確定するプロジェクトの中の文だけ。提案の時は見える出典の項目の名前と、§3.2 の決まりで丸めたまとめの数字だけ。**見えないプロジェクトの名前・項目の名前・引用は渡さない。** 機密の PBI・教訓（使う時に確かめる）は渡さない。プロンプトを組み立てる関数は 1 つにし、その入力の型が「見える出典」だけを受け取るようにする。
- **引用は出典のプロジェクトの中でだけ見せる。** 説明の文に他のプロジェクトの規則の引用を入れない（規則の種類と数字だけ）。
- **説明の文は実行に保存され、作った人以外も読む（再レビューで決定。両方を行う）。**
  1. 説明の文を書かせる時の入力は、**そのプロジェクトで `plan.estimate` を持つ全員（明示の PO / SM。管理者はすべて見えるので数えない）に見える出典の共通部分**だけにする。共通部分の外の出典は、LLM には「他のプロジェクト n 件（幅）」と丸めた数字だけを渡す。
  2. 実行には LLM に名前を渡した出典の ID の集合を残し、**読む時に、読む人に見える出典がこの集合を含まなければ説明の文を出さない**（決定的な根拠の表だけを出し、「AI の説明は表示できません」と 1 行出す）。メンバーが後から変わっても、2 が最後の守りになる。

**LLM の下書きの数字の扱い（設計レビューで決定）**: 下書きの規則は `factor`・`days`・`durationDays` などの数字を持つので、次を守る。
- 引用した文は、**出典の文の完全な部分文字列**でなければ捨てる（サーバーで照合。言い換えた引用は認めない）。出典の ID が入力に無いものも捨てる。
- `add_prerequisite` の `name` は 80 字まで。制御文字・改行・マークアップ・URL を除いた平文にし、表示ではいつもエスケープする。
- LLM の下書きの範囲は**いつも `project`**（§2.3。系統へ広げるのは人の昇格だけ）。
- 確定は 1 件ずつ、数字を出した状態で（§2.3）。上限（§2.3 の表）を超える数字は検証で外す。

### 3.5 負荷の再配分（平準化、B-1）

入力: 規則を当てた後の木、葉ごとの D50 と ℓ̂、役割の割り当て `roleAssignments: { role: userId[] }`（提案の時に PO / PM が選ぶ。`role` が無い葉は「担当未定」。**名前を挙げられるのはそのプロジェクトの明示のメンバー（`ProjectMember` の行がある人）だけ**で、それ以外の `userId` が 1 人でもあれば提案全体を 400 `PLAN_ROLE_ASSIGNMENT_INVALID` で断る。メンバーでない人を入れられると、平準化の日付と `moves` からその人の他のプロジェクトでの忙しい日が漏れるため。課長として見えるだけの人も明示のメンバーでなければ入れられない）、稼働量 C_u(d) = `member_capacity` の FTE（人の単位。このプロジェクトへの割り当ての行があればその値を上限にする）− u が**他のプロジェクトで**その日に持つ葉の ℓ の合計（0 未満は 0）、同時に持つ数の上限 m_u（過去の `ownerConcurrentPeak` の 80 パーセンタイルを既定 2 の方向に K で縮めて丸める。`cap_parallel` があればそれ）。

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
- **人の割り当ての変更は案に留める。** カードで 1 件ずつ外せる（外すだけ。足す・入れ替えるはできない。§3.6）。
- 同じ入力なら同じ結果（決め手に `userId` とテンプレートの並び順）。
- 負荷はリポジトリではなく人と葉で数える。

### 3.6 出力と承認（既存の流れを使い回す）

- 平準化の結果は**日付ではなく制約として**スケジュールに書く: `BaseScheduleItem` に任意の `notBeforeOffsetDays` を足し、`scheduleFrom` に「この日より前に始めない」を足す（E4）。承認の時の `POST /plan/load` は今までどおりサーバーで日付を計算し直すだけで、カードの日付を信用しない約束を守れる。
- **担当はスケジュールに書かない（設計レビューで決定）。** 担当を決めるのは平準化の出力だけで、`plan_estimate_run` の中に `key → userId` として置く。`schedule.yml`（外部のリポジトリのものを含む）やカードに `ownerId` があっても**サーバーは無視する**。カードから受け付けるのは**担当を外す操作だけ**（`removedOwnerKeys: key[]`）で、足す・入れ替える値は受け取らない。外した葉は「担当未定」で書く。
- カード: 既存の `plan_load` に任意の `estimate`（`runId`、方式の版、テンプレートとの差の要約、終了日: テンプレート / P50 / P80、負荷の最大: 前 / 後、データの量の 1 行）。**項目ごとの根拠は `plan_estimate_run` に置く**（55,000 バイトの上限）。「根拠を見る」から T_i・D50・D80・n・α・比・出典・規則の ID と引用・MR セットのリードタイムとリポジトリの内訳を開く（見せ方の制限は §3.2 と §4）。
- **`plan_estimate_run` を読める人（設計レビューで決定）**: 同じプロジェクトの中だけ。読むには**そのプロジェクトで `plan.estimate`** が要る。人ごとの部分（平準化の担当・`moves`・人ごとの使用率と重なる日）は、さらに `plan.viewMemberLoad` が要り、無ければその部分を応答から外す。道は `/projects/:projectId/plan/estimate-runs/:runId` だけで、`run.projectId ≠ :projectId` なら 404 `PLAN_ESTIMATE_RUN_NOT_FOUND`（他のプロジェクトの実行があるかどうかも教えない）。`/plan/load` も `estimateRunId` について同じく `run.projectId == パスの projectId` を確かめ、違えば同じ 404 にする。
- 承認: `/plan/load` は `estimateRunId` を受け、次のどれかなら 409 `PLAN_ESTIMATE_STALE`（黙って違う計画を書かない）:
  1. **入力のハッシュが違う。** 実行の時に保存した `inputHash` = ハッシュ（確定した規則の ID と版、使ったスナップショットの ID の集合（使う時の機密の確認の後）、テンプレートの `scheduleHash`、基準日 R（§3.1）、割り当てた人の `member_capacity` の行と他のプロジェクトでの割り付け、`roleAssignments`、方式の版）を承認の時に計算し直し、違えば古い。
  2. **実行の寿命が切れた。** `expiresAt` = 作った時 + 24 時間。
  3. 念のため平準化をやり直し、終了日か担当が変わった。
  書き込みで `templateKey`・`scheduleLoadId`・基準・`ownerId`（実行の出力から外した分を除いたもの）・`durationDays = D50` を保存する。
- 入口: Web の全体計画の「計画を作る」と、MCP の新しい道具 `propose_plan_from_actuals`（スコープ `backlog:write` に加えて、トークンの持ち主がそのプロジェクトで `plan.estimate` を持つこと。応答は `runId` と要約だけで、**人ごとの負荷は返さない**）。

## 4. 安全と個人の情報

| 項目 | 決まり |
|---|---|
| 機密の PBI（D-5） | すべて機密の葉は学習から外す。一部が機密ならその PBI の時間と MR を入れない。**機密かどうかは凍結せず、使う時ごとに今の印を確かめる**（§2.2）。機密の教訓は規則の下書きに入れない。MCP と LLM に渡さない |
| 見積もりの実行（`plan_estimate_run`） | 同じプロジェクトの中だけ、`plan.estimate` が要る。人ごとの部分はさらに `plan.viewMemberLoad`。`run.projectId` とパスの `projectId` が違えば 404（§3.6） |
| 保存した根拠の読み直し（再レビューで決定） | 実行に保存した根拠（出典ごとの比・MR の事実・リポジトリの内訳）は、**読む時に `learnableItemFacts` を通し直す**。実行の後に機密になった PBI の分は表から除き、除いた出典がその葉の見積もりに効いていたら、比と日数を出さず「根拠の一部が機密になったため表示を減らしました」と出す。入力のハッシュは使う時の機密の確認の後のスナップショットの集合を含むので、その実行の承認は 409 `PLAN_ESTIMATE_STALE` になる（§3.6） |
| LLM の説明の文 | 入力は `plan.estimate` を持つ全員に見える出典の共通部分だけ。読む人に見える出典が LLM に渡した集合を含まなければ出さない（§3.4） |
| 他のプロジェクトの実績 | 名前を出すのは `project.view` を持つときだけ（読む人の権限でその都度）。見えないものは「他のプロジェクト n 件（幅）」とまとめ、**葉ごとに見えない出典が 3 件未満なら使わない**。差分で 1 件を割り出させないため**2 件ずつ進め**、比は **0.1 単位に丸め**、出典ごとの行（MR のリードタイム・n・kept / dropped を含む）は出さない（§3.2、Q3） |
| リポジトリの内訳 | リポジトリの名前と差分の大きさを出すのは、**読む人が GitStation でそのリポジトリを読めるときだけ**。Kanflow は GitStation の専用の道 `POST /api/v1/integrations/{id}/visibility-check`（G1、再レビューで決定）に、読む人と対象のリポジトリをまとめて問い合わせる（I1 の `prove_visible` と同じ判定。1 回の表示で 1 回）。この道は**その連携のサービスアカウントだけ**が呼べ、返すのはリポジトリごとの真偽だけ、呼び出しは監査の記録に残す（§6 の G1）。確かめられない（結び付けた身元が無い・GitStation が応答しない・時間切れ）ときは**隠す**側に倒し、「リポジトリ n 本」とだけ出す。見えないプロジェクトの出典では常に出さない |
| 他の人の負荷 | 新しい操作 `plan.viewMemberLoad`: 本人、プロジェクトの PO / SM、課長。見せるのは使用率と重なる日だけで、**他のプロジェクトの項目の名前は出さない**（Q2） |
| 役割の割り当て | `roleAssignments` に挙げられるのはプロジェクトの明示のメンバーだけ（§3.5、`PLAN_ROLE_ASSIGNMENT_INVALID`） |
| 担当の決め方 | 平準化の出力だけが担当を決める。外部の `schedule.yml` とカードの `ownerId` は無視し、カードからは外す操作だけを受ける（§3.6） |
| 新しい操作（`policy.ts`） | 下の「権限の決まり」。どれも allow と deny の行を `policy.test.ts` に足す |
| LLM | 社内の Ollama だけ。**頼んだ人が見られるものだけを渡す**（見えないプロジェクトの名前・引用は渡さない）。引用は出典の文の完全な部分文字列だけ。下書きは `project` の範囲だけ（§3.4）。入力のハッシュ・モデル・プロンプトの版を `plan_estimate_run` に残す |
| 根拠の画面の人 | 過去の実績の根拠（出典・比・MR の内訳）には担当の ID・名前を**出さず、役割の名前だけ**を出す（見える出典でも同じ）。このプロジェクトの平準化の担当・`moves`・使用率は、`plan.viewMemberLoad` を持つ人にだけ見せる |
| 稼働量のメモ | `member_capacity` に自由記述の欄を**持たない**（健康・休みの理由が入るのを防ぐ。休みの理由を入れる場所を作らないのが最も確実なため、画面の注意書きと字数の上限より安全な側を選んだ）。期間と FTE だけ |
| 保持（Q8 は **E1 の着手前に決める**） | 既定の案: 実行（`plan_estimate_run`）は**1 年**で消す。反応（`plan_estimate_feedback`）も**1 年**で消す。バックテストの記録（人の情報を持たない）は残す。スナップショットは学習に使う間は残し、**古い行の担当の ID を役割の名前に置き換える仮名化**を許す |
| 仮名化（追記だけの例外） | スナップショットは行を書き換えない約束（§2.2）の**ただ 1 つの例外**。専用の操作（管理者だけ。`isSystemAdmin`）で、`plan_actuals_item.ownerId` を役割の名前（`role:<name>`）に置き換える。変えるのは担当の列だけで、数字は変えない。実行した人・日時・対象の件数を監査の記録（`ActivityLog`）に残す。退職者と保持期間を過ぎた行が対象。この例外は Kanflow の DB の文書にも書く |

**権限の決まり（`policy.ts`。設計レビューで決定）**: Kanflow の判定は**プロジェクトごとの操作**（`can(actor, action)`、許す相手は `po`・`sm`・`member`・`section_manager`、管理者はすべて）なので、プロジェクトの中の操作は `PROJECT_ACTIONS` に足す。**人の単位・系統の単位の判定は `can()` では表せない**ので、同じ `policy.ts` に純粋な関数を足し、判定の場所を 1 つに保つ。

| 判定 | 許す | 許さない（deny の行） |
|---|---|---|
| `plan.estimate`（プロジェクト） | `po`・`sm` | `member`（dev）、`section_manager` だけの人、メンバーでない人 |
| `plan.actualsFinalize`（プロジェクト。`includeInLearning` の切り替えも含む） | `po`・`sm` | `member`、`section_manager` だけの人 |
| `plan.viewMemberLoad`（プロジェクト） | `po`・`sm`・`section_manager`。**本人の分**は `actor.userId == 対象の userId` で別に許す | `member` が他の人の分を見る |
| `plan.lessonRule.confirm`（`project` の範囲の規則） | 出典のプロジェクトの `po`・`sm` | `member`、他のプロジェクトの PO / SM |
| 系統・種類の範囲の規則の確定と昇格の確定（`canConfirmLineageRule(actor)`） | **管理者だけ**（「系統の持ち主」の考えができるまで。Q6）。昇格の申請は出典のプロジェクトの `po` | 管理者でない人すべて（PO / SM・課長を含む） |
| `capacity.manage`（プロジェクト。`member_capacity` の `projectId` = このプロジェクトの行、つまりこのプロジェクトへの割り当て） | `po`・`sm` | `member`、他のプロジェクトの PO / SM が自分のでないプロジェクトの割り当てを変える |
| 人の単位の稼働量（`projectId = null` の行。`canManageUserCapacity(actor, targetUserId, targetSectionIds)`） | 本人、その人が属する課の課長、管理者 | PO / SM（自分のプロジェクトのメンバーであっても）、他の課の課長、本人以外のメンバー |

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
| **E1 [KF]** `feat/L4-plan-actuals-snapshot` | §2.2 と §2.4 の表、「実績を確定」、確定の前の検査、純粋な関数（日数・滑り・待ち・同時に持つ数・負荷・MR セットのリードタイム）、機密の除外、`project_attribute`、`GitLink` の MR の項目 | `snapshot_counts_workdays_when_skip_weekends`、`incomplete_leaf_is_kept_but_not_learnable`、`confidential_pbi_hours_are_excluded`、`refinalize_supersedes_without_update`、`concurrent_load_stores_no_other_project_names`、`on_hold_days_from_activity_log`、`mr_set_lead_time_spans_all_bound_repositories`、`closed_unmerged_mr_not_in_set`、`per_repo_breakdown_sums_to_item_totals`、`unreadable_repository_gives_null_not_zero`、`same_pbi_in_two_repos_counted_once_as_set`、`newly_confidential_pbi_excluded_at_use_time`、`deleted_pbi_treated_as_confidential`、`item_facts_only_via_learnable_item_facts`、`include_in_learning_toggle_requires_actuals_finalize`、`pseudonymize_replaces_owner_with_role_only_and_is_audited`、`pseudonymize_denied_for_non_admin`、権限の行（`plan.actualsFinalize` の allow / deny） | `typescript-reviewer`、`database-reviewer`、`security-reviewer`、`silent-failure-hunter` | K3 の後（**Q8 を決めてから**） |
| **E2 [KF]** `feat/L4-plan-estimate-engine` | `lib/plan-estimate/`（照合・重み・外れ値・縮め・P50 / P80・待ち・全体のバッファ）、バックテスト（リポジトリの数の層別を含む）と `plan_estimate_eval` | 性質のテスト: `no_observations_returns_template`、`estimate_moves_toward_data_as_n_grows`、`outlier_beyond_3_mad_is_dropped_and_reported`、`p80_never_below_p50`、`ratio_clamped_to_bounds`、`future_projects_never_used`、`same_input_same_output`、`backtest_reports_mape_by_repo_count`、`start_date_sweep_does_not_isolate_hidden_source`、`reference_date_is_run_month_not_start_date`、`hidden_contribution_is_trimmed_mean_not_single_source_value`、`one_new_hidden_source_does_not_change_estimate`、`type_prior_applies_hidden_source_rules`、`lag_load_and_parallel_cap_apply_hidden_source_rules`、`days_computed_from_rounded_ratio_when_hidden_influenced` | `typescript-reviewer`、`pr-test-analyzer` | E1 の後 |
| **E3 [KF]** `feat/L4-plan-lesson-rules` | `plan_lesson_rule`、6 種類の適用と検証、確定の流れと画面 | `draft_rule_never_changes_plan`、`rule_creating_cycle_is_rejected_with_reason`、`added_items_capped`、`scale_product_bounded`、`rules_apply_in_fixed_order`、`retro_rule_is_project_scope_only`、`rule_with_newly_confidential_source_stops_outside_project`、`promotion_strips_quote`、`lineage_rule_confirm_admin_only`、`rule_confirm_is_one_at_a_time_with_numbers`、権限の行（`plan.lessonRule.confirm`・`canConfirmLineageRule` の allow / deny） | `typescript-reviewer`、`database-reviewer`、`security-reviewer` | E2 と並行 |
| **E4 [KF]** `feat/L4-plan-capacity-leveling` | `member_capacity`、m_u の学習、§3.5 の平準化、`scheduleFrom` の `notBefore`、`plan.viewMemberLoad`・`capacity.manage` | `critical_path_item_scheduled_first`、`no_user_exceeds_capacity_after_leveling`、`zero_load_item_consumes_no_capacity`、`split_suggested_only_when_it_finishes_earlier`、`unassigned_role_leaves_owner_null`、`not_before_respected_by_server_recompute`、`member_cannot_see_others_load`、`po_sees_utilization_but_not_other_project_names`、`role_assignment_outside_project_rejected`、`section_manager_not_member_cannot_be_assigned`、`capacity_has_no_free_text_note`、`user_capacity_allowed_for_self_section_manager_admin_only`、`po_cannot_set_user_level_capacity`、`po_sets_only_own_project_allocation`、権限の行（`plan.viewMemberLoad`・`capacity.manage`・`canManageUserCapacity` の allow / deny）、500 件で 2 秒以内 | `typescript-reviewer`、`security-reviewer`、`database-reviewer`、`pr-test-analyzer` | E2 の後 |
| **E5 [KF]** `feat/L4-plan-propose-from-actuals` | `plan_estimate_run`、Web の「計画を作る」、MCP `propose_plan_from_actuals`、カードと根拠の画面、`/plan/load` の再検査と `PLAN_ESTIMATE_STALE`、反応の記録、系統ごとの既定の切り替え | `card_fits_size_limit_with_500_items`、`approval_recomputes_and_refuses_stale_plan`、`stale_when_input_hash_changes`、`stale_after_run_lifetime`、`mcp_response_has_no_per_person_load`、`mcp_requires_plan_estimate`、`run_of_another_project_is_refused`、`load_with_run_of_another_project_is_refused`、`per_person_parts_need_view_member_load`、`invisible_sources_under_3_per_leaf_are_not_used`、`hidden_sources_advance_two_at_a_time`、`hidden_source_ratio_never_shown`、`shown_ratios_rounded_to_0_1`、`evidence_names_resolved_with_reader_permissions`、`stored_evidence_refiltered_at_read_time`、`newly_confidential_source_hides_leaf_ratio_and_makes_run_stale`、`llm_explanation_hidden_from_reader_seeing_fewer_sources`、`repo_names_hidden_for_invisible_source_projects`、`repo_names_need_gitstation_read_access`、`repo_names_hidden_when_gitstation_unreachable`、`evidence_shows_roles_not_owner_ids`、`card_cannot_add_owners`、`external_schedule_owner_id_ignored`、`proposal_contains_only_template_or_rule_items`、`same_proposal_loads_once`、`gate_off_lineage_shows_template_with_reason`、権限の行（`plan.estimate` の allow / deny） | `typescript-reviewer`、`security-reviewer`、`a11y-architect`、`i18n-sync-auditor`、`silent-failure-hunter` | E3・E4 の後 |
| **E6 [KF]** `feat/L4-plan-estimate-llm` | 規則の下書き（引用つき）、段 3・4 の候補の採否、説明の文。止まったら決定的な部分に戻る | `llm_timeout_falls_back_and_is_logged`、`invalid_llm_output_retried_once_then_dropped`、`llm_cannot_emit_numbers_outside_rule_schema`、`injected_retro_text_does_not_change_rules`、`confidential_lessons_not_in_prompt`、`llm_input_contains_only_viewer_visible_data`、`hidden_candidates_not_sent_to_llm`、`llm_quote_must_be_exact_substring`、`llm_rule_name_capped_and_sanitized`、`llm_drafts_are_project_scope`、`explanation_has_no_quotes_from_other_projects`、`llm_explanation_input_is_intersection_of_estimators_visibility`、`run_records_source_ids_given_to_llm`、規則の下書きの評価セット | `llm-integration-reviewer`（M5 で追加の予定）、`security-reviewer`、`typescript-reviewer` | **M5**（L-5 と同じ段） |
| G1 `feat/cross-dev-status-mr-timing` | 開発状況 API の MR に `opened_at`・`merged_at`・`closed_at`・`version_count`・`diff_stats` を足す（足すだけ）。結んだ全リポジトリの MR を返す。イベントの履歴の `merge_request.opened` / `merged` が時刻を持つか確かめる（**未確認**。無ければ足す）。**読む人の閲覧の確認の道 `POST /api/v1/integrations/{id}/visibility-check`**（再レビューで決定）: 本文は（GitStation の利用者の参照 1 人、リポジトリの参照 100 件まで）、応答は `{ repo: bool }` だけ（理由・存在・権限の種類を返さない。無いリポジトリと知らない利用者は `false`）。`forge-auth` の `authorize()` で**その連携のサービスアカウントだけ**を許し、他の利用者・他の連携のサービスアカウント・エージェントのトークン・代理（act-as）での呼び出しは断る。権限の表に 1 行（allow / deny）。呼び出しごとに監査の記録（呼んだサービスアカウント・対象の利用者・件数・`request_id`）を残し、速度の上限を付ける | `dev_status_reports_opened_and_merged_at`、`version_count_matches_merge_request_versions`、`diff_stats_absent_when_not_computed_is_null_not_zero`、`dev_status_returns_mrs_from_every_bound_repository`、`mr_in_repository_caller_cannot_read_is_omitted`、`visibility_check_allowed_only_for_integration_service_account`、`visibility_check_denied_for_other_users_and_other_service_accounts`、`visibility_check_denied_for_agent_tokens`、`visibility_check_denied_under_act_as`、`visibility_check_returns_booleans_only`、`unknown_repo_and_unknown_user_return_false`、`visibility_check_matches_prove_visible`、`visibility_check_is_audited`、権限の表の行 | `rust-reviewer`、`authz-matrix-auditor`、`security-reviewer`、`database-reviewer`、`pr-test-analyzer`、`error-contract-auditor` | I1 の後 |

**エラーコード**: GitStation の G1 は既存のコードで足りる見込み（閲覧の確認の道の拒否は既存の認可の拒否のコード。新しい失敗が要れば同じ MR で `errors.toml` に登録する）。Kanflow は `PLAN_ACTUALS_INCOMPLETE`（400）、`PLAN_RULE_INVALID`（400）、`PLAN_ROLE_ASSIGNMENT_INVALID`（400）、`PLAN_ESTIMATE_STALE`（409）、`PLAN_ESTIMATE_RUN_NOT_FOUND`（404。他のプロジェクトの実行もこれにする）。LLM の失敗は利用者に見せず、記録して元の表示に戻す（07 W14）。

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
| 他のプロジェクトの数字が漏れる | `project.view`（読む人の権限で都度）、葉ごとに見えない出典 3 件以上・2 件ずつ進める・刈り込み平均・比を 0.1 に丸めて日数も丸めた比から・出典ごとの行を出さない・μ0 などの集計にも同じ決まり・基準日は実行の月で固定、リポジトリの名前は GitStation で読める人だけ、実行は同じプロジェクトの中だけ |
| 後から機密になった PBI・教訓が使われ続ける | 機密を凍結せず、使う時に今の印を結合して確かめる（PBI の ID を葉ごとに残す） |
| 平準化から外の人の忙しい日が漏れる | `roleAssignments` はプロジェクトのメンバーだけ |
| LLM の規則が計画を壊す | 下書きは効かない、確定は人で 1 件ずつ数字を見せて、6 種類だけ、検証で外す、引用は完全な部分文字列だけ、下書きは `project` の範囲だけ |
| LLM から見えない文が漏れる | 頼んだ人が見られるものだけを渡す、振り返りはプロジェクトの内部の扱い、昇格で引用を消す |
| M5 の LLM が L-5 と取り合う | 同時実行の枠を分け、E6 のまとめ処理は夜間 |
| カードの日付と書き込みの日付が違う | 制約として渡しサーバーで計算し直す。入力のハッシュ・24 時間の寿命・再計算のどれかが違えば 409 |
| カードや外部の `schedule.yml` で担当をすり替える | 担当は平準化の出力だけ。カードからは外すだけ |
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
2. **他の人の負荷を誰が見られるか**。推奨: 本人、そのプロジェクトの PO / SM（使用率と重なる日だけ）、課長。稼働量を入れるのは、人の単位の FTE は本人・その人の課長・管理者、プロジェクトへの割り当ては PO / SM（§4 の権限の決まり。設計レビューで決定）。
3. **見えないプロジェクトの実績を使ってよいか**。推奨: 使う。名前を出さず、葉ごとに 3 件以上（2 件ずつ進めるので実際は 4 件以上）を刈り込み平均でまとめ、比を 0.1 に丸めたときだけ。基準日は実行の月で固定（§3.1・§3.2。仕組みは設計レビューと再レビューで決定）。残る問い: 部署をまたいでよいか（課の範囲に限るか）。
4. **全体のバッファを計画の行として入れるか**。推奨: 入れない。P50 と P80 の終了日を並べる。約束の日付は P80 に合わせる運用を勧める。
5. **設備・工程のマスタの正本**（IoT の PFS の DB か、別の台帳か）。決まるまでは自由な文字列。
6. **反省の規則を確定できる人**。設計レビューで決定: プロジェクトの規則は出典のプロジェクトの PO / SM。系統・種類の規則と昇格は**管理者だけ**（「系統の持ち主」の考えを Kanflow に作るまで）。残る問い: 系統の持ち主を作るか、作るなら誰か（テンプレートの持ち主など）。
7. **既定の数値**: K = 3、半減期 365 日、比 0.5〜3.0、外れ値 3 MAD。推奨: この値で始め、バックテストで見直す。
8. **保持と退職者**（**E1 の着手前に決める**。設計レビューで決定）。推奨: スナップショットはプロジェクトと同じ期間残し、退職した人と期間を過ぎた行は管理者の仮名化の操作で役割の名前に置き換える（追記だけの約束の、監査つきの例外。§4）。実行と反応は 1 年で消す。
9. **AI の案を既定で見せる条件**。推奨: 系統ごとに、終わったプロジェクト 5 件以上・MAPE 15% 以上の改善・P80 の当たり 70〜90%。平準化は最初から使える。
10. **複数のリポジトリの MR の数え方**。推奨: PBI ごとの MR セットを単位に、「最初に開いた日から最後のマージまで」。マージしないで閉じた MR は数えない。
11. **仕様の改訂（`spec-drift`）を実績に残すか**。推奨: 残すが、見積もりには使わず根拠に出すだけ。
12. **時期**。推奨: E0 は今すぐ、E1〜E5 は M3 後半〜M4、E6 は M5（L-5 と同じ段）。G1 は I1 の後。

## 10. 設計レビューで決めたこと（2026-11-02）

セキュリティの設計レビューの指摘と決定。権限は Kanflow v2 の `policy.ts`（プロジェクトごとの操作、許す相手は `po`・`sm`・`member`・`section_manager`、管理者はすべて）に合わせた。

| 指摘 | 決定 | 節 |
|---|---|---|
| HIGH 1: `plan_estimate_run` を誰が読めるかが決まっていない | 同じプロジェクトの中だけ、`plan.estimate` が要る。人ごとの部分（平準化の担当・`moves`・使用率）はさらに `plan.viewMemberLoad`。`/plan/load` と読む道は `run.projectId == パスの projectId` を確かめ、違えば 404 `PLAN_ESTIMATE_RUN_NOT_FOUND`（存在も教えない）。MCP もトークンの持ち主の `plan.estimate` を確かめる | §3.6、§4 |
| HIGH 2: `roleAssignments` に外の人を入れると忙しい日が漏れる | プロジェクトの明示のメンバー（`ProjectMember`）だけ。違えば 400 `PLAN_ROLE_ASSIGNMENT_INVALID`。課長として見えるだけの人も不可 | §3.5、§4、§6 |
| HIGH 3: 見えない出典の「3 件以上」の単位・差分での割り出し | 葉ごとに数える。**2 件ずつ進める**（見えない出典は確定の古い順に、数を偶数に切り下げた件数だけ使う）ので 1 件の追加では見積もりが変わらない。比は 0.1 に丸め、n は幅で出す。見えないプロジェクトの出典ごとの行（MR のリードタイム・n・kept / dropped を含む）は出さない。名前は読む人の権限でその都度解決し、読む人に 4 件未満ならまとめの比も出さない | §3.1、§3.2、§4 |
| HIGH 4: LLM に見えない文が渡る・引用が漏れる | LLM には頼んだ人が見られるものだけ（見えない名前・引用は渡さない、見えない候補は式だけで採否）。引用は出典のプロジェクトの中だけ。機密の出典を持つ規則は `project` の範囲だけ（機密の教訓・PBI は LLM には渡さないまま）。振り返りはプロジェクトの内部の扱いで、振り返りからの規則は `project` の範囲だけ。系統へ広げるのは PO の申請と管理者の確定で、そのとき引用を消す（指摘の「PO が昇格」は、系統の規則を管理者だけとする MEDIUM の決定と合わせ、申請と確定に分けた） | §2.3、§3.4、§4 |
| HIGH 5: `confidential` を凍結すると後から機密にしたものが使われ続ける | **使う時に確かめる**方を選んだ。葉ごとに PBI の ID と PBI ごとの値を `plan_actuals_item_pbi`・`plan_actuals_item_repo` に残し、使う時に今の `confidential` を結合して計算する（`learnableItemFacts` の 1 か所だけ）。消えた PBI は機密として扱う。規則も出典の ID を持ち、使う時に確かめる。スナップショットを作り直す方式は、印が変わるたびに確定し直しが要り、漏れを防ぐのが確定の操作に頼ることになるので採らない | §2.2、§2.3、§2.4、§3.1、§4 |
| MEDIUM: LLM の下書きが数字を持つ | 引用は出典の文の完全な部分文字列だけ。`name` は 80 字までの平文。下書きは `project` の範囲だけ。確定は 1 件ずつ、数字を見せて（まとめての確定は無い） | §2.3、§3.4 |
| MEDIUM: 古い案の判定 | 保存した入力のハッシュ（規則・使ったスナップショット・テンプレートのハッシュ・稼働量と他のプロジェクトの割り付け・割り当て・方式の版）を比べ、実行の寿命は 24 時間。加えて再計算で終了日か担当が変われば 409 | §3.6 |
| MEDIUM: 担当のすり替え | 担当は平準化の出力だけ。カードからは外す操作だけを受け、外部の `schedule.yml` とカードの `ownerId` は無視する | §3.5、§3.6、§4 |
| MEDIUM: 人の単位・系統の単位の権限 | プロジェクトの中は `PROJECT_ACTIONS`、人・系統は `policy.ts` の純粋な関数（`canManageUserCapacity`・`canConfirmLineageRule`）。人の単位の FTE は本人・その人の課の課長・管理者、プロジェクトへの割り当ては PO / SM、系統の規則は管理者だけ。allow / deny の行を足す | §2.1、§4、§6、§9 |
| MEDIUM: 根拠の MR の事実 | リポジトリの名前と差分の大きさは、読む人が GitStation でそのリポジトリを読めるときだけ（読む人の身元で GitStation に問い合わせる）。確かめられなければ隠して「リポジトリ n 本」 | §4 |
| MEDIUM: 保持と個人の情報 | Q8 は E1 の前に決める。仮名化は管理者の専用の監査つきの操作で、追記だけの約束のただ 1 つの例外（担当の列だけ）。実行と反応は 1 年。根拠に担当の ID を出さず役割だけ。`member_capacity` の自由記述の欄は**持たない**（注意書きと字数の上限より確実なため） | §2.1、§2.2、§4、§9 |
| LOW: `includeInLearning` の切り替えの権限 | `plan.actualsFinalize`（PO / SM）の下。理由と操作した人を記録 | §2.2、§4、§6 |
| 再レビュー HIGH 3 (a): 種類の傾向 μ0 などの集計に見えない出典の決まりが無い | μ0・`lag`・ℓ̂・m_u・§5 の件数も、見えない出典の分は集計の単位ごとに 3 件以上・偶数に切り下げ・刈り込み平均・出典ごとの行を出さない、を当てる | §3.2、§3.3 |
| 再レビュー HIGH 3 (b): 出す日数から正確な比を逆算できる | 見えない出典が効いた葉は比を 0.1 単位に丸め、D50 / D80 は**丸めた比から**計算する。正確な比からの日数はカード・根拠・`durationDays`・平準化・B のどこにも使わない | §3.2 |
| 再レビュー HIGH 3 (c): 開始日を動かして境目の 1 件を割り出せる | 照合の締め切りと重みの経過日数は、**実行を作った日の月の 1 日（基準日 R）**から決める。提案する人の開始日は使わない。R は入力のハッシュに入れる。バックテストだけは評価するプロジェクトの開始月 | §3.1、§3.6、§6 |
| 再レビュー HIGH 3 (d): 重みつき中央値は 1 件の出典の値そのものになる | 見えない出典はプロジェクトごとに 1 つの値 y_p にし、最大と最小を除いた平均 x_H = (Σ y_p − max − min) / (h' − 2)、重み w_H = (h' − 2) × 見えない出典の w の平均の擬似の観測 1 件にまとめる。見える出典は中央値のまま | §3.2 |
| 再レビュー MEDIUM N2: LLM の説明の文が作った人の見え方で保存される | 両方を行う: 入力は `plan.estimate` を持つ全員（明示の PO / SM）に見える出典の共通部分だけにし、さらに読む時に読む人に見える出典が LLM に渡した集合を含まなければ説明の文を出さない（メンバーの変更への最後の守り） | §3.4、§4、§6 |
| 再レビュー MEDIUM N3: 「読む人として」リポジトリの閲覧を確かめる道が無い | G1 に `POST /api/v1/integrations/{id}/visibility-check` を足す。その連携のサービスアカウントだけ、真偽だけ、監査つき、権限の表に allow / deny の行（他の利用者・エージェント・act-as は拒否）。評価役に `security-reviewer` を足す | §4、§6 |
| 再レビュー MEDIUM: 保存した根拠が実行の後の機密の変更を反映しない | 読む時に `learnableItemFacts` を通し直し、機密になった分を除く。効いていた葉は比と日数を出さず、その実行の承認は 409 | §4、§6 |
| 最終の再レビュー MEDIUM: バックテストの結果（`plan_estimate_eval` の MAPE・P80 の当たり・終了日の誤差）に見えないプロジェクトが含まれ、確定の前後の差で 1 件の誤差が分かる | `plan_estimate_eval` を読めるのは管理者と、その系統の全プロジェクトを見られる人だけ。ほかの人（PO / SM への「悪くなった」通知を含む）には丸めた幅だけを見せ、見えないプロジェクトを含む系統では 2 件そろうごとにしか更新しない | §5 |
| 最終の再レビュー LOW: 確定し直しや PBI の機密化で、見えないプロジェクト 1 件の y_p だけが変わる（件数は変わらない） | 値の変化も件数の変化と同じに扱い、見えない出典の値が変わったプロジェクトが 2 件そろうまで、前の集計を使う | §3.2 |
