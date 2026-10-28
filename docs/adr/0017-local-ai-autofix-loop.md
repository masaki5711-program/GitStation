# ADR-0017: ローカル LLM による MR の自動修正ループと、kanflow からの承認・マージ

**Date**: 2026-10-28
**Status**: accepted（ユーザー決定 2026-10-26〜28、`security-reviewer` + `authz-matrix-auditor` の 3 回のレビュー。**ADR-0003 と ADR-0004 を改訂する**（§4.1.5）。ADR-0006 の「AI による衝突の解消案は別ブランチ」の一文も改訂する。ADR-0010 の `Principal` / `Credential` / `ProjectView` のロールの読み方とスコープの表し方を拡張する（§4.0、§4.4）。ADR-0015 の登録の仕組みに `InstanceRoute::Confirm`、`INTERNAL`、`required_scopes()` を足す（§4.0））
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## Context

ADR-0003 は「LLM には変更系の道具を渡さず、出力が操作を起こすことはない」とし、ADR-0006 と `04` §4.3 は
「失敗を機械が読める診断 API（P-9）で返し、**外部の AI エージェント**が読んで直す」とした。しかし利用者の全員が
エージェントを持つわけではなく、AI レビュー（`01` L-2）の指摘も、対象ブランチとの衝突も、人が手で直すまで MR は止まる。
また、仕様（要件 ID と受入基準、ADR-0004）を参照して MR を照合する仕組みは助言のチェックだけで、照合の結果を承認に使う道がない。
kanflow からの承認・マージは、保護ブランチで「本人の直接の資格情報」を要するため、kanflow の画面だけでは終えられなかった。
さらに、kanflow のプロジェクトのオーナー（PO など）が forge でマージするには、forge で別にメンバーとして登録される必要があった。

**ユーザーの決定（2026-10-26〜28、この ADR の前提で、変えない）**

1. 修正のコミットは **MR のソースブランチに直接** 積む。対象は自動修正を有効にした MR だけ（プロジェクトの設定 + MR ごとの無効化）。
   作成者は見て分かるボットの ID。保護ブランチ（ADR-0008）には決して書かない。
2. 直す対象は、パイプラインの失敗（build / lint / format / test。**決定的な修正を LLM より先に** 実行）、AI レビューの指摘（L-2）、対象ブランチとの衝突（L-5 を前倒し）。
3. マージは人間の承認 + 決定的な必須チェックのまま。AI は MR を「通る状態」にするだけ（ADR-0003 の「LLM の判断だけでマージ・デプロイしない」は維持）。
   **（決定 9 で補う: プロジェクトが選んだカテゴリでは AI（仕様照合）の承認を 1 件と数える。マージの実行は常に人間の操作で、自動マージはしない）**
4. **承認とマージは kanflow からもできる**。押すのは kanflow の画面の人間で、forge の画面と同じ規則に従う。
5. **サービスアカウント・エージェントが作った MR では、自動修正は既定で無効**（MR ごとに明示的に有効にする）。
6. **コミットの暗号署名は後回し**。今はトレーラー + DB の記録。トレーラーは偽装されうる前提で設計する。
7. **AI のコミットは常にすべての承認を取り消す**（プロジェクトの設定で変えられない）。
8. エラーの新しい領域 `INT`、`LLM`、`REVIEW` を承認する（ゲートに追加する）。
9. **仕様照合のレーン**（2026-10-28、「推奨の仕様で進めてください」）:
   ① 決定的なチェック `forge/spec-trace`（**全プロジェクトで必須**）: MR が触れる要件ごとに `@req` のテストがあり、成功していること。
   ② AI の仕様照合 `forge/spec-conformance`（**全プロジェクトで必須チェック。承認ではない**）: 受入基準ごとに、差分 + テストが満たすかを LLM が根拠の行付きで判定する。
   **必須化の時期（2026-10-28 ユーザー確認）**: まず **影の段階（助言）** で動かし、測った基準を満たしたら必須にする。必須化の後は、**可用性の失敗の `error` のときだけ** 条件付きの人間の免除を設ける（§4.1.2）。
   ③ **AI の承認を 1 件と数えるのは、プロジェクトが明示的に選んだカテゴリだけ**（docs だけの変更、テストだけの追加、内部のミラーからの依存関係のパッチ更新、決定的な fmt / clippy の修正）。
   設定は対象ブランチの側から読み、MR の中で変えられない。保護パス・マイグレーション・CI の設定に触れる MR には出さない。保護ブランチでは人間の承認が別に 1 件以上要る。
   AI が書いたコミット（自動修正）を AI が承認しない（自己承認）。新しいコミットで人間の承認と同じく消える。承認の種類「AI（仕様照合）」として区別して表示し、根拠と入力を後から見直せるよう残す。
   ④ **自動マージは今はしない**。③の誤った承認の率を数か月測ってから、保護されていないブランチから再検討する。
10. **kanflow からのマージは A + D**（2026-10-28）:
   A. **確認の画面**: kanflow で承認を押すと forge の小さな確認の画面（forge のセッション / SSO）が開き、本人が MR の番号・コミット・変更の要約を見て 1 回押す。
   forge が 5 分・1 回限りで、メソッド + パス + `sha` に結びつけた主張を発行し、kanflow がそれを付けて送る。
   D. **マージの予約**: 「承認してマージを予約」で MR を forge のマージキューに入れ、CI が緑になったら forge が予約した本人としてマージする
   （マージの時点で本人の今のロールで認可し直し、その時点の `evaluate_merge()`、head の SHA が承認した SHA と同じこと。新しいコミットで予約は取り消す）。kanflow にはイベントで知らせる。
   採らない: B（個人のトークンを kanflow に保存）、C（保護されていないブランチは kanflow だけで完結）。
   kanflow の側の作業: 確認の画面を開く、主張を受け取って送る、完了の通知を受け取る。
11. **kanflow のプロジェクトのオーナーに forge のマージの権限を与える**（2026-10-28、「両方がマージできる」）: 結びつけた kanflow のプロジェクトで owner の人は、
   結びつけた forge のプロジェクトでマージできる。**forge で付けた Maintainer もそのままマージできる**（どちらか一方だけ・両方を要する、の案は採らない）。
   与えるのはロールではなく **マージ・承認・キュー・予約・読み取りの能力の許可リスト**（§4.4。決定の「マージできる」の範囲の中）。

**脅威のモデル**: MR の作成者はもともと自分のブランチに push できるので、作成者がボットを使って得るものは少ない。主な脅威は
(a) **作成者以外が書いた文**（ジョブのログ、依存関係、対象ブランチ、AI レビュー、要件）をボットが読んで、範囲外の変更・情報の持ち出しに誘導されること、
(b) **侵害された Runner** が候補に別の変更を混ぜること、(c) 代理実行で **1 つのサービスアカウントが多くの人の承認を作る** こと、
(d) **差分に書いた文で仕様照合の LLM を誤らせ**、AI の承認を得て人間のレビューなしにマージさせること、(e) **侵害された kanflow** が主張を作る・使い回す・本文を差し替える・本人にクリックの時機を狙って押させる・本人に別の MR を確認させること、
(f) **侵害された（または誤設定の）kanflow** がオーナーの一覧を書き換えて forge の権限を作る・広げる・残すこと、kanflow や worker の停止で権限の取り消しが届かないこと、
(g) **エージェントと宣言されたトークン・サービスアカウントのトークン** が AI の承認と組み合わさり、人間が一度も関わらずにマージすること。

制約: モデルは qwen3.8 27B を kanflow と共用の Ollama GPU で動かす（`01` §6.6）。リポジトリのコードを forge のサーバーで実行しない（`04` §5）。
ADR-0015（accepted）の `prove()` / `Authorized<R>`、1 ルート = 1 Action に従う（コードにはまだない。本 ADR はその実装を前提にする）。イベント・監査は ADR-0016 / ADR-0018（審査中）と整合させる。
Claude が新しいセッションで、ログと `request_id` だけから「なぜ AI がこのコミットを積んだか / なぜ AI が承認したか / 誰がどの経路・どのロールの出どころでマージしたか」を辿れること。

## Decision

### 0. 全体の流れ

ユーザーの要約（2026-10-28）: 「GitStation で使用するローカル AI で衝突及びパイプラインの確認を行い自動で修正を行う。通ったらマージ前まで持っていき、GitStation からマージするか Kanflow からマージを行う」

```
MR（作成・push）
  │
  ▼
パイプライン（マージ結果）+ 衝突の確認（merge-tree）
  │
  ├─ 失敗・衝突 ─▶ ローカル AI の自動修正（§1〜§3）
  │                  ① 決定的な修正（fmt / clippy --fix / ロックファイル）を先に
  │                  ② 直らなければ LLM（道具なし、置換の JSON だけ）
  │                  → 候補で検証し、通ったものだけ MR のブランチにボットのコミットとして積む
  │                  → 承認はすべて取り消し（決定 7）→ もう一度確認へ（上限・予算で止まり、止まれば MR にコメント）
  │
  ▼ 緑
仕様照合（§4.1）: ① forge/spec-trace（決定的、必須）+ ② forge/spec-conformance（LLM。影の段階の後に必須）
  │               ③ 選んだカテゴリだけ AI（仕様照合）の承認 1 件
  ▼
「マージできる」状態で止まる（evaluate_merge() = Mergeable）── AI はマージしない（自動マージは保留、§4.1.6）
  │
  ▼ 人がマージする
  ├─ GitStation（forge の画面。セッション）
  └─ kanflow（PBI の画面）── A: forge の確認の画面で 1 回押す → 1 回限りの主張（§4.2）
                           └─ D: 承認してマージを予約 → CI が緑で本人としてマージ（§4.3）

マージできる人: forge の Maintainer と、結びつけた kanflow のプロジェクトのオーナー（同期した能力、§4.4）。
エージェントと宣言されたトークン・サービスアカウントのトークンはマージできない（§4.0）。
どの経路も同じ「MR の行をロック → prove → evaluate_merge() → execute_merge()」を通る。
```

### 方針

自動修正を **「認可 → 候補の tree を作る → サーバーがコミットを作り検証する → 候補でパイプラインを通す → ロックの中で前提を確かめ直して早送りで積む」**
決定的な状態機械として実装する。LLM は **道具を持たず**、構造化された置換の案（JSON）を返すだけ。ADR-0003 の「出力が操作を起こすことはない」を
「**出力が操作を起こすのは自動修正だけで、コードが検証し、隔離された Runner で適用し、サーバーが作ったコミットが決定的なチェックを通った場合に限る**」に改める。
マージ可否は **権限（`authorize()`）と MR の状態（`evaluate_merge()`）の 2 つの純粋な関数** で決め、マージを実行する関数は 1 つだけで、両方の証明を引数に取る。
**すべての git の起動とリポジトリの読み取りの前に `prove()` を通す**（自動修正の ④、AI の承認の分類も同じ）。
自動修正のボットは承認もマージもできない。承認できるボットは仕様照合のボット（§4.1）だけで、承認の種類が別で、選ばれたカテゴリでだけ数え、マージは実行しない。
kanflow は「マージしてよい人間」の代理としてだけ、許可リストの操作ができ、変更の操作のたびに本人が forge の確認の画面で押した主張を要する（§4.2）。
kanflow のオーナーは、forge が同期して持つ **能力の許可リスト**（マージ・承認・キュー・予約・読み取り）だけを得る。他の Action はすべて forge の所属のロール（`native_role`）で決め、直接の push は与えない（§4.4）。
`authorize()` は判定の時点で kanflow を呼ばず、同期の鮮度もロールを読む SQL の中で判定する（ジョブの稼働に依存しない）。

### 1. ループ

```
trigger（Outbox の購読者 autofix-trigger）
  pipeline.failed（MR の最新の head）/ check.completed（forge/ai-review の HIGH 以上）/ merge_request.conflict_detected
  → 前提の検査: MR が open、自動修正が有効（§2.6）、ソースが同じリポジトリの保護されていないブランチ、マージキューにない、予算・試行の残り、
    同じ trigger の試行が進行中でない、緊急停止でない → 満たさなければ記録して終了（skipped / gave_up）
  → autofix_attempts に 1 行（state=queued、enabler、入力の一覧）。ジョブ autofix.run（dedup_key = mr:<id>:<trigger_key>:<head_sha>）
① 決定的な修正（LLM なし）: 対象ブランチ側の `.forge-ci.yml` の `autofix.fixers`（fmt → `cargo fmt --all`、lint → `cargo clippy --fix`、C-5 のロックファイルの再生成）
② ①がない・直らない → 入力の組み立て（worker）: すべての入力を Principal::autofix（§2.5）で prove(Read*) してから読む（§2.3）
   → 秘密情報の検出と伏せ字（§2.4）→ LLM ゲートウェイ（§3）→ JSON スキーマで検証（不正なら 1 回だけ再試行）
③ 候補の tree（Runner のジョブ kind=autofix。秘密情報なし、環境・デプロイなし、ネットワークは内部のミラーの許可リストだけ）:
   Runner は **ジョブのトークン**（`Credential::JobToken { job, repo }`。このリポジトリの `RepoRead` だけ。upload-pack の通常の認可を通る）で head_sha を checkout
   → 置換の適用（中間のシンボリックリンクを辿らない）→ パスの検査 → bundle を返す
④ サーバー（forge-git）: **先に** MR の行の状態（DB）だけで作る `AutofixTarget` に対して `prove(AutofixCandidateWrite)` を通す（git もリポジトリも読む前）。
   通ったら bundle を隔離領域に展開（`transfer.fsckObjects`、サイズと時間の上限、前提は {head_sha}（衝突なら + target_sha）だけ）。
   **bundle の中のコミットは運搬用で、tree だけを使う**。サーバーが `git commit-tree` で親（通常は head_sha、衝突は head_sha と target_sha）、
   ボットの ID、決まった形のメッセージとトレーラーを付けてコミットを作る → `diff-tree -r --no-renames --raw` で §2.1 を検査（衝突は §1.2 も）
   → 候補の ref `refs/forge/autofix/<mr_id>/<attempt_id>`（隠し ref。書き込みは上の証明で行う）
⑤ 候補で MR のパイプライン（M-1 のマージ結果。環境・デプロイ・シークレットなし）
⑥ 判定: 失敗の指紋が真に減り新しい失敗がない、§2.2 のテストの条件を満たす（指摘なら再レビューで消え新しい HIGH 以上がない）
⑦ 反映（1 つの ChangeTx）: MR の行を `FOR UPDATE` でロック → 前提をすべて確かめ直す（作成者と enabler が有効でロールがある、ソースの ref が今もこの MR のもの、
   保護の規則（大文字小文字を区別しない一致）、緊急停止、マージキュー、自動修正が有効）→ prove(AutofixPush) → `update-ref <src> <candidate> <head_sha>`（比較して交換）
   → 監査 + `merge_request.pushed` + `merge_request.unapproved`（reason=autofix_commit）+ `merge_request.autofix.applied` → 候補の ref を消す
   ⑥を満たさない → 候補を捨てる（ref を消す。autofix.discarded、reason = regression / no_progress / tests_weakened）
```

#### 1.1 人間の優先、競合、再利用

- ソースブランチに人間（またはエージェント）が push したら進行中の試行は `superseded`。⑦の比較と交換の失敗も同じ。**人間のコミットを上書きせず、force push しない**。
- 積んだコミットは候補と同じ SHA なので、候補で得たチェックの結果がそのまま head に付く。push のイベントによるデプロイは自動修正の反映では動かさない。
  **候補のパイプラインで走らなかった必須チェック（シークレットを要するジョブなど）は head に付かず、受け継がない**。反映の後に、それらのジョブだけを通常の MR のパイプラインで走らせ、結果が出るまでマージは止まる（blocker `required_check_missing`）。
- P-9 に `autofix: {state, attempt_id, next_action}` を足し、外部のエージェントと競合しない。

#### 1.2 衝突（悪意のあるマージを防ぐ）

- rebase はしない。サーバーが `merge-tree --write-tree head target` を自分で計算し、候補の tree が自動のマージと違ってよいのは **衝突したパスの衝突した hunk の中だけ** とする。
  その差分に §2.1 を適用する。緑になる候補だけを積み、それ以外（緑にならない、マイグレーション・バイナリ・保護パスの衝突）は解消案を MR のコメントで出すだけ（autofix.proposed）。
- 「AI 変更」タブはマージのコミットを `--remerge-diff` で見せる（自動のマージとの違いだけが見える）。

### 2. 安全

#### 2.1 出力の形とパスの規則

- 出力: `{edits: [{path, old_text, new_text}], rationale, addresses: [fingerprint | finding_id | conflict_path]}`。`old_text` がちょうど 1 回現れなければ拒否。
  **ファイルの削除と、ファイルを空にする置換は拒否**。`new_text` に bidi 制御・ゼロ幅の文字があれば拒否（Trojan Source）。
- **編集してよい範囲**: MR の差分に既にあるファイルだけ。例外は、**対象ブランチで定義されたジョブ** が出した **構造化された診断**（rustc / clippy の JSON、
  eslint の JSON など、パーサーを登録した形式）が名指ししたファイル。**自由な文のログから範囲を広げることはない**。上限 10 ファイル・200 行。
- **固定の拒否**（設定で緩められない）: `.forge-ci.yml`、`.forge/**`、`CODEOWNERS`、`.gitattributes`、`.gitmodules`、`.gitignore`、`build.rs`、`**/Cargo.toml`、`.cargo/**`、
  `rust-toolchain*`、`rustfmt.toml`、`clippy.toml`、`deny.toml`、`package.json`、`.npmrc`、`Makefile`、`Dockerfile*`、`.vscode/**`、`.idea/**`、`.envrc`、
  `.pre-commit-config.yaml`、`.husky/**`、`CLAUDE.md`、`AGENTS.md`、`**/SKILL.md`、`.cursorrules`、`.claude/**`、`scripts/hooks/**`、プロンプトの置き場所、**対象ブランチの `.forge-ci.yml` が読み込む・実行するすべてのファイル**
  （対象ブランチの版から計算）、秘密情報らしいパス（`.env*`、`*.pem`、`*.key`、`id_*`、`*secret*`）。モードは 100644 の通常のファイルだけ（120000 / 160000 / 100755 への変更・追加を拒否）。
- **マイグレーション**: 対象ブランチに存在するファイルは変更不可（CLAUDE.md §3-6）。**保護パス**（§2.7）は変更不可。
- **パスの正規化**: `.git` の変種（`.GIT`、`git~1`、末尾の `.` や空白）、`\`、NUL、大文字小文字だけの違い、リポジトリの外を拒否する（ADR-0009 / ADR-0015 の規則と同じ関数）。
- 規則は **プロジェクトの設定（DB）と対象ブランチ** からだけ読む。MR の tree からは読まない。範囲外の試みは監査に `denied` で残す（攻撃の兆候）。

#### 2.2 テストを弱めない、進歩の判定

- ジョブごとに **実行したテストの件数**（JUnit）が head より減らないこと、**head で失敗したテストの ID が「成功」として現れる**（スキップ・欠落は不可）ことを⑥の条件にする。
  スキップの印（`#[ignore]`、`#[cfg(any())]`、`.skip(`、`@pytest.mark.skip`）の追加とテスト関数の削除は §2.1 の段階でも拒否する。
- パイプラインの失敗の trigger では、**失敗したテストのファイルの編集を既定で拒否**（テストのコンパイルエラーは除く）。期待値の変更は「AI 変更」タブで強調する。

#### 2.3 入力の権限（LLM を介した IDOR を防ぐ）

- 入力（ジョブのログ、差分、指摘、要件、P-7 の既知の直し方）は、すべて `Principal::autofix` で `prove(RepoRead / JobLogRead / MergeRequestRead / RequirementRead …)` を通してから読む。
  ボットの主体の判定では、ローダーが **`on_behalf_of`（作成者）と `enabler` の `ProjectView` を両方載せ**、`authorize()` は両者の `native_role` の小さい方で判定する（ボット自身の所属は使わない）。
  P-7 は **同じプロジェクトか、作成者が読めるプロジェクト** のものだけ。
- 試行の状態（段階、結果、理由）は `AutofixAttemptRead`（Reporter 以上）。**入力と出力の本文** は `AutofixAttemptInputRead` で、入力の出どころごとの最も厳しい読み取りの権限を
  すべて満たす人だけが読める（Resource が出どころの一覧を持つ）。パスの `{aid}` がパスの MR に属さなければ 404（ctx の構築で確かめる）。
- 読み取りの Action `JobLogRead`（ジョブのログ、Reporter 以上、`read_api`）、`PipelineRead`（パイプラインとチェックの結果、Reporter 以上、`read_api`）、`RequirementRead`（仕様のプロジェクトの要件、
  そのプロジェクトの Reporter 以上、`read_api`）は **本 ADR で Action と最小のロールを定義** し、ルートは CI の ADR（M4）と仕様の ADR（M3）で足す。それまでは `app::PLANNED` に置き、
  ボット・システムの内部の読み取りの行と、deny の行（見えないプロジェクト、Guest、匿名）を持つ。

#### 2.4 秘密情報

- LLM への入力は、ログの画面と同じ伏せ字に加え、秘密情報の検出（forge のトークンの接頭辞、PEM、高いエントロピーの文字列）を通す。
- 候補の差分と `rationale` にも同じ検出をかけ、見つかれば拒否（`secret_in_output`）。保持するデータは暗号化し、§2.3 の規則でだけ読める。

#### 2.5 ボットの主体と認可

- ボット `forge-autofix` は **資格情報を持たない**（ログインもトークンもない。`Credential::Internal`）。ユーザー名と email（`autofix@forge.invalid`）は予約する。
  pre-receive（ADR-0008）は、**人間の push のコミットの author / committer がボットの ID、または `Forge-Autofix-` のトレーラーを持つもの** を拒否する。
- 主体は worker だけが作る `Principal::autofix(bot, mr, on_behalf_of: 作成者, enabler)`。`Principal` は種類 `Kind::{Human, Bot(BotKind), Service, System(SystemWorker)}` を持ち、明示のコンストラクタでだけ作る（§4.0）。
  権限は **ボット ∩ 作成者 ∩ enabler（作成者以外が有効にした場合）**。
- `authorize()` はボットに対して **許可リストから始める**。許可リストは `BotKind::allows(Action) -> bool` で、**`_` のない全列挙の match**（forge-authz の規則 3）:
  `Autofix` は `AutofixPush`、`AutofixCandidateWrite`、`AutofixComment`、`AutofixAttemptRead`、`RepoRead`、`MergeRequestRead`、`JobLogRead`、`PipelineRead`、`RequirementRead`。それ以外はすべて deny。
  マトリクスの行は `for a in Action::ALL` × ボットで **生成** し、許可リストの外は deny を確かめる（手書きしない）。
- `Resource::AutofixTarget { project, name, protection, mr_open, autofix_enabled, source_is_same_repo, author_role, author_active, enabler_role }`（すべて DB から。git を読まずに作れる）:
  ブランチ以外の ref の種類は deny、`protection` が Some なら管理者でも deny、`mr_open` / `autofix_enabled` / `source_is_same_repo` が false なら deny、
  作成者・enabler とも `need(Developer)`（`native_role`）。行: 作成者が無効、作成者がロールを失った、enabler がロールを失った、保護ブランチ、別のリポジトリのソース。
- 承認（人間の `MergeRequestApprove` と AI の `MergeRequestAiApprove` の両方）、マージ、マージキュー、予約、確認の主張の発行、スレッドの解決、保護・必須チェック・CI・自動修正の設定、
  トークン、代理実行の対象になることは、上の生成された deny の行に含まれる。

#### 2.6 有効化、コミットの見え方、戻し方、再レビュー

- プロジェクトの有効化は `AutofixSettingsManage`（**`native_role` で Maintainer 以上**）。MR ごと（`MergeRequestAutofixToggle`）: 作成者は自分の MR で有効・無効、**他人の MR を有効にするのは `native_role` で Maintainer 以上**、無効にするのは Developer 以上。
  有効にした人（enabler）を記録し、§2.5 の共通部分に入れる。サービスアカウント・エージェント（`agent` 属性のトークン）が作った MR は既定で無効（決定 5）。
- コミットのメッセージは決まった形（`autofix(<trigger>): <短い要約>`）で、トレーラーは `Forge-Autofix-Attempt`、`-Trigger`、`-Fixer`、`-Model`、`-Prompt`、`Forge-Request-Id`。
  **`Signed-off-by` は付けない**（DCO の意味を持たせない）。真実の記録は DB の `autofix_commits`。LLM の文はコメントでもただの文として表示し、メンション・`closes` の語・画像の URL を除く。
- 「AI 変更」タブ: AI のコミットだけの差分（マージは `--remerge-diff`）、理由、対応した指紋・指摘、期待値の変更の強調、revert のボタン、「役に立った／誤り」。
- revert（`AutofixCommitRevert`）は `Resource::Ref` と `RepoPush` と **同じ判定の関数** を使い（`native_role`）、押した本人の名前でコミットする。保護 ref には deny。パスの `{sha}` がその MR の `autofix_commits` になければ 404。
- **承認は head の SHA に結びつける**（構造で取り消す）。承認の行は `sha` と **種類 `kind`**（`human` / `ai_spec`、§4.1）を持ち、`WHERE head_sha = $sha` で保存する。`evaluate_merge()` は **今の head と同じ `sha` の承認だけ** を数える。
  人間の push でプロジェクトが承認を残す設定なら、その push の ChangeTx が人間の承認を新しい SHA へ写す（`carried_from`）。**AI のコミットの反映では写さない**（決定 7）。**`ai_spec` の承認はどの push でも写さない**（§4.1.3）。
  イベント `merge_request.unapproved`（reason=autofix_commit、主体はボット）は⑦と同じ ChangeTx で出す（ADR-0016）。
- 自己承認: `Resource::MergeRequest { project, author, ai_commit_present, head_sha_matches, committers, enabler, … }`。`MergeRequestApprove` は `user == author` を
  AI のコミットの有無に関係なく deny（reason=self_approval）。**enabler と、MR にコミットを push した人間** の承認は、プロジェクトの設定「コミッターの承認を数えない」（既定で有効）のとき数えない。

#### 2.7 保護パス、リポジトリの中の設定、CODEOWNERS

- **保護パス**は DB のプロジェクトの設定 `protected_paths`（パスのパターンの一覧。例 `crates/forge-auth/**`、権限マトリクス）で、`ProtectedPathManage`（`native_role` で Maintainer 以上が追加、削除・狭めるのは `native_role` で Owner）で管理し、監査に残す。
  ADR-0008 は保護 ref だけを定義するので、保護パスはここで定義する。§2.1、§4.1.3 の除外、下の blocker が同じ判定の関数を使う。
- **リポジトリの中の設定のファイル**（`.forge/**`、`.forge-ci.yml`、`CODEOWNERS`、対象ブランチの `.forge-ci.yml` が読み込む・実行するファイル、保護パス）を変える MR は、
  `evaluate_merge()` が blocker `native_maintainer_approval_required` を出す。これを満たすのは、**評価の時点で `native_role` が Maintainer 以上の人間の承認** だけで、
  kanflow の出どころの能力、`ai_spec`、作成者・コミッター・enabler の承認は数えない（`ApprovalRow.approver_native_role` は評価の時点でローダーが読む）。
- **CODEOWNERS のロールの指定**（`@<project>/maintainers` のようにロールやグループで書いた所有者）は **`native_role` で解決** する。kanflow の出どころの能力は CODEOWNERS の所有者にならない（個人の名前で書かれた所有者は本人の承認で満たす）。
- **保護ブランチの設定 `required_native_approvals`**（0 以上、既定 0）: 1 以上なら、`native_role` が設定の最小のロール（既定 Developer）以上の人間の承認をその数だけ要する（blocker `native_approval_required`）。
  kanflow の出どころの能力の承認と `ai_spec` は数えない。プロジェクトは「kanflow のオーナーの承認だけでは保護ブランチにマージさせない」ことを選べる。

### 3. 停止の条件と資源

| 条件 | 既定（プロジェクトで下げられる。上げるのは管理者） | 結果 |
|---|---|---|
| MR あたり / trigger あたり | 6 回 / 24 時間、2 回 | `gave_up reason=max_attempts_mr` / `max_attempts_trigger` |
| 進歩なし / 退行 | 同じ指紋の集合が 2 回 / 新しい失敗が 2 回 | `no_progress` / `regression` |
| 予算 | MR あたり GPU 30 分・Runner 60 分 / 日、プロジェクトあたり 30 試行 / 時、**利用者あたり自動修正が有効な MR 10 件** | `budget` |
| インスタンス全体 | **自動修正の推論は同時に 1 つ**、入力 16k・出力 4k トークン、推論ごとのタイムアウト 120 秒、GPU 時間の 25 % まで、Runner は低い優先度か別のプール | 待つ → 期限で `gpu_unavailable` |
| 期限 | trigger から 2 時間 | `deadline` |
| 優先度 | **対話 > MR レビュー（`forge/spec-conformance` を含む）> 自動修正 > 一括処理**。実行中の推論を奪わない。**MR レビューの層には GPU 時間の最低 20 % を予約し、10 分待った推論は対話と同じ優先度に上げる**（飢餓を防ぐ） | — |
| 緊急停止 | インスタンス: `AutofixPause` / `AutofixResume`（管理者 + `admin` スコープ、本番では承認済みの Runbook の手順）。プロジェクト: `AutofixSettingsManage` で無効にする | 新しい試行を作らず、進行中は `superseded` |

`gave_up` は必ず MR にコメント（理由、最後の診断、次に人がすべきこと）で見せ、黙って止まらない。

### 4. 承認とマージの経路（kanflow の代理実行、AI の承認、確認の画面、予約、kanflow のオーナー）

#### 4.0 主体、資格情報、許可リスト、スコープ（ADR-0010 / ADR-0015 の拡張）

- **`Kind`**: `Human`、`Bot(BotKind::{Autofix, SpecLane})`、`Service`（サービスアカウント自身として呼ぶとき）、`System(SystemWorker::{MembershipSync, SpecTrace})`。主体は明示のコンストラクタでだけ作る。
- **`Credential`**（ADR-0010 の `Session` / `Token` を拡張）:

| 資格情報 | 作る場所 | 許す Action（`_` のない全列挙の述語。外は `for a in Action::ALL` で deny の行を生成） |
|---|---|---|
| `Session` | ログイン | 本人のロールの範囲のすべて |
| `Token { scopes, agent: bool }` | 個人のトークン | スコープ ∩ ロール。**エージェントと宣言されたトークン（`agent = true`）は `Action::is_merge_family()`（`MergeRequestMerge`、`MergeQueueEnqueue`、`MergeReservationCreate`、`MergeRequestApproveAndReserve`、`MergeSetRunCreate`）と `MergeRequestApprove` を deny** |
| `ActAs { via, via_scopes, assertion: Option<VerifiedAssertion> }` | 代理（kanflow） | `Action::act_as_allowed()` だけ: `ProjectRead`、`MergeRequestRead`、`MergeSetRunRead`、`MergeRequestApprove`、`MergeRequestUnapprove`、`MergeRequestMerge`、`MergeQueueEnqueue`、`MergeReservationCreate`、`MergeRequestApproveAndReserve`、`MergeReservationCancel`、`MergeSetRunCreate`。**読み取り以外は、Action に対応する intent の主張が必須**（§4.2）。`02` / `03` で計画中の代理の操作（ブランチの作成、結びつけの登録）は代理実行の ADR で許可リストに足す |
| `Reserved { reservation, origin }` | 予約の worker（§4.3） | `MergeRequestMerge` だけ |
| `Queued { run, origin }` | マージキュー・MR セットの worker | `MergeRequestMerge` だけ |
| `JobToken { job, repo }` | Runner のジョブ | その `repo` の `RepoRead` だけ |
| `Internal` | ボット・システムの worker | `BotKind::allows()` / `SystemWorker::allows()` だけ（全列挙） |

  サービスアカウント自身（`Kind::Service`、代理でない）は `is_merge_family()` と `MergeRequestApprove` を deny（人間でない主体はマージも承認もしない、脅威 g）。
  `agent` の印は **トークンの発行時に宣言されたもの** で、宣言しないエージェントのトークンは人間のトークンと区別できない（残る危険。§Risks）。
  代理の対象は人間だけで、**`Principal::act_as` のコンストラクタ** がボットとサービスアカウントを拒否する（`FG-INT-0006` は `authorize()` の前に出るので、マトリクスではなくコンストラクタの単体テストで確かめる）。
  サービスアカウントとプロジェクトの結びつけは `ProjectView.via_bound: bool`（純粋に判定）。本人が管理者でも、`admin` スコープがなければ管理者の権限を持たない（トークンと同じ）。
- **スコープの表し方**: 既存の `Action::required_scope() -> Option<Scope>` を **`required_scopes() -> ScopeRequirement`** に置き換える（全列挙）。
  `ScopeRequirement::{None, All(&'static [Scope]), Any(&'static [Scope]), SessionOnly}`。例: `MergeRequestApproveAndReserve` = `All([MergeApprove, MergeExecute])`、`MergeAssertionClaim` = `Any([MergeApprove, MergeExecute])`、
  `MergeConfirmView` / `MergeAssertionIssue` / `TokenCreate` ほか = `SessionOnly`。`requires_session()` は `SessionOnly` から導く。**`Api` スコープは `MergeApprove` / `MergeExecute` を満たさない**（テストで確かめる）。
- **ロールの読み方**: `ProjectView.role` のフィールドをやめ、アクセサ `native_role()`（forge の所属: プロジェクトの直接の所属と祖先のグループの最大値）と `role_for(action) -> (Option<Role>, RoleSource)` に分ける。
  `role_for` は `action.kanflow_grant_allowed()`（§4.4）のときだけ kanflow の出どころの能力を含めた最大値を返し、それ以外は `native_role()` を返す。`authorize()` の各 arm はどちらかを明示的に選ぶ（コンパイルで強制）。
  同じ値なら出どころは `Forge`。所属の変更（`MemberManage`）の `MembershipChange.from` と、呼び出した人の上限のロールは `native_role()` で判定する。
- **ボットの読み取りの範囲（ローダーの規則）**: `Bot(SpecLane)` の主体に対して、ローダーは **MR のプロジェクトと、対象ブランチ（MR の target の SHA）の `.forge/repo.yml` の `specs:` で固定された仕様のプロジェクトだけ** の
  `ProjectView` を、読み取りだけ（Reporter 相当、`role_for` は読み取りの Action だけに効く）で載せる。他のプロジェクトは view がなく 404。`Bot(Autofix)` は §2.3 のとおり作成者 ∩ enabler。
- **セッションだけの Action**（`SessionOnly`）: `MergeConfirmView`、`MergeAssertionIssue`、`IntegrationMembershipSyncToggle`、`IntegrationMembershipSyncAcknowledge`、`IntegrationBindingManage`、`SpecConformanceWaive`、`TokenCreate`。セッション以外はすべて deny（生成した行）。
- **内部の操作**: ルートを持たない Action は `app::PLANNED`（ルート未実装）ではなく **`app::INTERNAL: &[Action]`** に置く: `AutofixPush`、`AutofixCandidateWrite`、`AutofixComment`、`MergeRequestAiApprove`、`InternalCheckWrite`、`MembershipSyncApply`、`MembershipSyncSuspend`。
  ゲート `authz-coverage` は「`INTERNAL` の Action はルートに現れず、マトリクスの行を持ち、worker のテスト（記録する偽の authorizer）で `prove()` が呼ばれる」ことを確かめる。
  ルートが後の ADR で付く Action（`JobLogRead`、`PipelineRead`、`RequirementRead`、Checks API の `CheckRunWrite`（`02` §5.4、kanflow の作業項目チェック））は `app::PLANNED` に置き、deny の行を持つ。
- **登録**: 確認の画面は `InstanceRoute::Confirm`（語 `confirm`、`WebShell` でない）として `scoped()` に載せる。`/-/confirm/**` は、パスの存在に関係なく、未認証には同じログインへの誘導を返す（戻り先は同じ origin のパスだけ、検証する）。

- **代理実行の ADR の受理までの最低の要件**（`02` §5.3 で ADR と `security-reviewer` が必須）: 代理の変更の操作には本人ごとの **1 回限りの主張**（§4.2）を要する（記録は ADR-0016 の `via_assertion_id`）。**主張は forge だけが確認の画面で発行する**。
  **保護ブランチでは、必要な承認の少なくとも 1 つか、マージそのものが本人の直接の資格情報（セッションか個人のトークン、または §4.2 の主張）から来ること**（1 つのサービスアカウントが N 人分の承認を作れないようにする）。
- **同じ規則**: 代理の承認は本人の承認として数え、自己承認の禁止も本人で判定する。承認の数、CODEOWNERS、§2.6 の SHA に結びついた承認、§2.7 の blocker、必須チェックと鮮度（M-2）、スレッド、
  作業項目のリンク、保護ブランチ、マージキュー（M-3）、§4.1 の AI の承認の数え方は、画面と同じ `evaluate_merge()` で判定する。経路ごとの分岐はない。
- **権限と状態を分ける、順序を 1 つにする**: `authorize()` は主体の権限だけを決め、`evaluate_merge(&MergeState)` は MR の状態だけの純粋な関数で、**主体の種類を引数に取らない**。
  **すべての経路（画面・REST・代理・予約・MR セット・マージキュー）は「MR の行をロック → `prove()` → `evaluate_merge()` → `execute_merge()`」の順** にそろえる。
  `execute_merge(Authorized<MergeRequestCtx>, MergeGateOk, LockedMr)` は 1 つだけで、対象ブランチを `update-ref <target> <new> <target_sha>`（`MergeGateOk` の `target_sha` との比較して交換）で更新する。
  **マージの出どころ** `MergeOrigin { principal_kind, credential_kind, agent_declared, via, reservation_id?, queue_run_id?, role_source, credential_origin }` は、`execute_merge` が証明から作り、
  `merges` の行・`MergeOutcome`・監査・イベントに記録する。**`MergeState` には入れない**（`evaluate_merge()` が主体の種類で分岐しないことを型で保つ。人間でない主体のマージの拒否は `authorize()` が行う）。
  `MergeGateOk` は MR の行をロックした中で `evaluate_merge()` からしか作れず、head と target の SHA を持つ。`mergeability` の `viewer_can_merge` は、共通のヘルパー（`authorize(MergeRequestMerge, …)` を呼ぶ）だけで計算する。

- **REST（`/api/v1`、ADR-0015 の 1 ルート = 1 Action、ハンドラは `Authorized<R>` を受け取り、ID を証明から取る）**。「代理」の列は許すかと、要する主張の intent:

| メソッド / パス | Action | ResourceCtx | スコープ（`required_scopes()`） | 代理 | 内容 |
|---|---|---|---|---|---|
| `GET …/merge_requests/{iid}/mergeability` | `MergeRequestRead` | `MergeRequestCtx` | `read_api` | 可（主張なし） | `blockers: [{kind, count}]`、`head_sha`、`viewer_can_merge`、`reservation: {state, sha}?`。blockers は Reporter が MR の画面で既に見られる状態だけ |
| `POST …/merge_requests/{iid}/approvals` | `MergeRequestApprove` | `MergeRequestCtx` | `MergeApprove` | 可（`approve`） | 本文 `{sha}`。head と違えば 409 `MergeHeadMismatch` |
| `DELETE …/merge_requests/{iid}/approvals/me` | `MergeRequestUnapprove` | `MergeRequestCtx`（本人の承認） | `MergeApprove` | 可（`unapprove`） | `ai_spec` の承認はこのルートで消せない |
| `POST …/merge_requests/{iid}/merge` | `MergeRequestMerge` | `MergeRequestCtx` | `MergeExecute` | 可（`merge`） | `{sha, method}`。代理では `commit_message` を受け付けない（既定の文）。キューが必要なブランチは 409（`queue_required`） |
| `POST …/merge_requests/{iid}/merge_queue` | `MergeQueueEnqueue` | `MergeRequestCtx` | `MergeExecute` | 可（`enqueue`） | `{sha, method}`。投入は同じロックを取り、head が変わったら外す |
| `POST …/merge_requests/{iid}/merge_reservation` | `MergeReservationCreate` | `MergeRequestCtx` | `MergeExecute` | 可（`reserve`） | §4.3。`{sha, method}` |
| `POST …/merge_requests/{iid}/approve_and_reserve` | `MergeRequestApproveAndReserve` | `MergeRequestCtx` | `All(MergeApprove, MergeExecute)` | 可（`approve_and_reserve`） | §4.3。承認と予約を 1 つの ChangeTx で。`authorize()` は承認と予約の両方の規則を 1 つの arm で判定し、どちらかが deny（自己承認など）なら全体が deny（部分的に成功しない） |
| `DELETE …/merge_requests/{iid}/merge_reservation` | `MergeReservationCancel` | `MergeRequestCtx`（`reserved_by`） | `MergeExecute` | 可（`cancel_reservation`） | 予約した本人か `native_role` で Maintainer 以上 |
| `POST /work_items/{provider}/{ref}/merge_set_runs` | `MergeSetRunCreate` | `MergeSetCtx { steps: Vec<Resource::MergeRequest> }` | `MergeExecute` | 可（`merge_set`、手順の一覧のハッシュ） | `{steps: [{project, iid, sha, method}]}`。`authorize()` が全手順の MR を merge の規則で判定し、1 つでも見えなければ 404（FG-REPO-0005） |
| `GET /merge_set_runs/{id}` | `MergeSetRunRead` | `MergeSetRunCtx`（全手順のプロジェクト） | `read_api` | 可（主張なし） | 全手順のプロジェクトを読めなければ 404 |
| `POST /integrations/{id}/merge_assertions/claim` | `MergeAssertionClaim` | `IntegrationCtx` | `Any(MergeApprove, MergeExecute)` | 不可（サービスアカウント自身） | §4.2 の裏の経路。本文 `{state}`。応答は主張と `sub`（1 回限り） |
| `GET …/merge_requests/{iid}/approvals/{approval_id}/evidence` | `AiApprovalEvidenceRead` | `AiApprovalCtx`（入力の出どころ） | `read_api` | 不可 | §4.1.4。`approval_id` がパスの MR に属さなければ 404 |
| `POST …/merge_requests/{iid}/approvals/{approval_id}/feedback` | `AiApprovalFeedback` | `AiApprovalCtx` | `api` | 不可 | 事後のレビュー（`native_role` で Developer 以上。作成者・コミッター・enabler は自分の MR に deny） |
| `PUT /projects/{id}/ai_approval/settings` | `AiApprovalSettingsManage` | `ProjectCtx` | `api` | 不可 | DB のカテゴリの有効化（`native_role` で Maintainer 以上）。実効 = DB ∩ 対象ブランチのファイル |
| `POST …/merge_requests/{iid}/checks/spec-conformance/waive` | `SpecConformanceWaive` | `MergeRequestCtx` | `SessionOnly` | 不可 | §4.1.2 の可用性の `error` のときだけの免除 |
| `PUT /projects/{id}/protected_paths` | `ProtectedPathManage` | `ProjectCtx` | `api` | 不可 | §2.7 |
| `POST` / `PATCH` / `DELETE /integrations/{id}/bindings[/{binding_id}]` | `IntegrationBindingManage` | `BindingCtx`（forge のプロジェクト、同期の有効・無効） | `SessionOnly` | **不可**（`03` §7.1 の代理の登録は代理実行の ADR で決める） | 結びつけの登録・変更・解除（`native_role` で Maintainer 以上。同期が有効な結びつけの変更・解除は `native_role` の Owner）。kanflow の出どころは deny。ルートは M3 で付くまで `PLANNED` |
| `GET …/merge_requests/{iid}/autofix/attempts` | `AutofixAttemptRead` | `MergeRequestCtx` | `read_api` | 不可 | 状態だけ |
| `GET …/merge_requests/{iid}/autofix/attempts/{aid}/io` | `AutofixAttemptInputRead` | `AutofixAttemptCtx`（入力の出どころ） | `read_api` | 不可 | §2.3 |
| `PUT /projects/{id}/autofix/settings` | `AutofixSettingsManage` | `ProjectCtx` | `api` | 不可 | プロジェクトの設定（無効化 = プロジェクトの停止） |
| `PUT …/merge_requests/{iid}/autofix` | `MergeRequestAutofixToggle` | `MergeRequestCtx` | `api` | 不可 | §2.6 |
| `POST …/merge_requests/{iid}/autofix/commits/{sha}/revert` | `AutofixCommitRevert` | `RefCtx`（`RepoPush` と同じ判定） | `write_repository` | 不可 | 本人の名前で revert |
| `POST /admin/autofix/pause` / `resume` | `AutofixPause` / `AutofixResume` | `Instance` | `admin` | 不可 | インスタンスの緊急停止 / 再開 |
| `POST /admin/ai_approval/pause` / `resume` | `AiApprovalPause` / `AiApprovalResume` | `Instance` | `admin` | 不可 | AI の承認の停止 / 再開 |
| `POST /admin/spec_conformance/degraded` | `SpecConformanceDegradedSet` | `Instance` | `admin` | 不可 | §4.1.2 の縮退の切り替え（承認済みの Runbook の手順） |
| `PUT /integrations/{id}/bindings/{binding_id}/membership_sync` | `IntegrationMembershipSyncToggle` | `BindingCtx` | `SessionOnly` | 不可 | §4.4。`native_role` で Owner だけ。有効化の本文に、画面で見せたオーナーの一覧のハッシュ |
| `POST /integrations/{id}/bindings/{binding_id}/membership_sync/acknowledge` | `IntegrationMembershipSyncAcknowledge` | `BindingCtx` | `SessionOnly` | 不可 | 保留した追加の確認（`native_role` で Owner） |
| `GET /integrations/{id}/bindings/{binding_id}/membership_sync` | `IntegrationMembershipSyncRead` | `BindingCtx` | `read_api` | 不可 | 最後の成功・失敗、人数、保留・突き合わせできない件数（email は出さない）。`native_role` で Maintainer 以上 |
| `POST /admin/integrations/{id}/membership_sync/pause` / `resume` | `IntegrationMembershipSyncPause` / `IntegrationMembershipSyncResume` | `Instance` | `admin` | 不可 | 同期の緊急停止。停止中は kanflow の出どころの能力をロールの SQL が数えない |

**画面のルート（`forge-web`、`InstanceRoute::Confirm`、`SessionOnly`）**:

| メソッド / パス | Action | ResourceCtx | 内容 |
|---|---|---|---|
| `GET /-/confirm/projects/{id}/merge_requests/{iid}?intent=…&sha=…&integration=…&state=…` | `MergeConfirmView` | `MergeAssertionCtx { intent, target: MergeRequest, integration, via_bound }` | §4.2 の確認の画面。MR が見えなければ `ProjectRead` と同じ 404（存在を漏らさない）。押せない理由は MR が見える人にだけ表示 |
| `POST /-/confirm/projects/{id}/merge_requests/{iid}/assertions` | `MergeAssertionIssue` | 同上 | 本文 `{intent, sha, method?, integration, state}` + CSRF。主張を発行し、`state` に結びつけて保存する |
| `GET /-/confirm/work_items/{provider}/{ref}/merge_set?…` / `POST …/assertions` | `MergeConfirmView` / `MergeAssertionIssue` | `MergeAssertionCtx { intent: MergeSet, target: MergeSet, … }` | MR セット用。全手順を表示し、手順の一覧のハッシュに結びつける |

（パスの `…` は `/projects/{id}`。MR の画面の URL は ADR-0015 の `/-/pull/{iid}` で、M3 の ADR で決める。`INTERNAL` の Action はルートを持たない。）

- **冪等性**: キーは（サービスアカウント, 本人, メソッド, ルートの型, `Idempotency-Key`）で 24 時間保存。**認可を先に行い**、今の認可が通るときだけ保存した応答を返す。
  同じキーで違う本文は 422。`sha` が必須なので再送が別の head をマージすることはない。既にその `sha` でマージ済みなら 200（`already_merged: true`）。
  主張は 1 回限りなので、同じ主張での再送は保存した応答を返すときだけ許す。
- **MR セットとマージキュー**: worker は `Credential::Queued { run }` の主体（保存した本人 + 経路）で **手順ごとに MR の行をロック → `prove(MergeRequestMerge)`**（本人の今のロール、サービスアカウントの今のスコープと結びつけ、元の資格情報の有効性（§4.3 と同じ））→ `evaluate_merge()`。
  最初の blocked / failed で止め、残りは skipped。リポジトリをまたぐ原子的なマージはできないので **マージ済みの手順は戻さない**（応答とイベントで明示し、M-4 の revert の下書きを使える）。認可されない手順は worker に届かない。
- **結果の伝え方**: 同期の応答 + イベントの履歴（ADR-0016、pull が正）に `merge_request.approved`（`approval_kind`）/ `unapproved`、`merged`（`merge_origin`）、`merge_queued`、**`merge_blocked`**（`reasons`）、
  **`merge_reserved`** / **`reservation_cancelled`**（§4.3）、`merge_set_run.step_completed` / `completed` / `stopped`。代理の操作は `actor_kind = user_via_service_account`（`actor_user_id` = 本人、`actor_via_id` = kanflow、`via_assertion_id` = 主張の ID）。
- **代理実行の ADR に残すこと**: email → ユーザーの対応と退職・無効化、トークンの発行・失効、流量制限、OAuth2 への移行、`02` / `03` の他の代理の操作（結びつけの登録を含む）を許可リストに足すこと、上の最低の要件の詳細。
  **その ADR が受理されるまで `MergeApprove` / `MergeExecute` を代理のサービスアカウントに付与できない**（代理の allow の行は `app::PLANNED`）。**代理の deny の行はすべて M3 で書いてテストする**（M3 の完了条件）。

#### 4.1 仕様照合のレーンと AI の承認（決定 9）

要件に触れる MR は、決定的な照合（①）と LLM の照合（②）の 2 つのチェックを通る。③は承認の 1 つの種類で、④の自動マージは保留する。
**マージの実行は常に人間の操作**（直接、代理、§4.3 の予約）で、`evaluate_merge()` を通る。**②は安全の制御ではない**（受入基準を満たしているかの補助で、悪意のある変更を止める役目は①、§2、§2.7、人間の承認が負う）。

##### 4.1.1 ① `forge/spec-trace`（決定的、全プロジェクトで必須）

- **MR が触れる要件** = MR の作業項目（PBI、`02` の連携で取得）が参照する要件 ID ∪ 差分で追加・変更された `@req` が指す要件 ID ∪ 差分が変えた `spec/` の要件ブロックの ID。
  要件 ID は `.forge/repo.yml` の `specs:` で固定した版から解決する（対象ブランチの版。MR の tree の版は読まない）。
- **合格の条件**: 触れた要件ごとに、**マージ結果の tree** に `@req <ID>` のテストが 1 つ以上あり、そのテストの ID が MR のパイプライン（今の head のマージ結果）の JUnit に **成功** として現れる。
  ない・スキップ・失敗・欠落 → `failure`（summary に `untested` / `failing` の一覧）。触れた要件がない MR は `success`（件数 0 を summary に出す）。`spec` の種類のリポジトリでは `forge/spec-lint` を同じく必須にする。
- 必須はシステムの規則で、プロジェクトの設定・保護の規則から外せない（ADR-0004 の「助言から始め、計測後に必須化を判断」を改訂、§4.1.5）。
- 純粋な関数 `spec_trace()` が決め、入力（要件の一覧、索引、テストの結果）のハッシュをチェックの結果に残す。結果を書くのは `Principal::system(SpecTrace)` で、
  `InternalCheckWrite`（`INTERNAL`。`Resource::CheckRun { project, name }`）は `name = forge/spec-trace` だけを許す。

##### 4.1.2 ② `forge/spec-conformance`（LLM。影の段階の後に必須チェック。承認ではない）

- 対象: ①で触れた要件の **受入基準**（要件ブロックの `受入:` と PBI の受入基準）ごとに、差分 + テストが満たすかを判定する。出力は JSON スキーマで検証する:
  `{criteria: [{criterion_id, verdict: satisfied | partial | no_evidence, evidence: [{path, start_line, end_line, side: diff | test}], reason}]}`。
- **根拠の検証（コード）**: 引用した行が、マージ結果の tree の差分かテストに実在し、**コメントでも文字列だけでもないコードかテストの行**（登録した言語の字句の規則で判定。登録していない言語は根拠にしない）でなければ、
  その受入基準は `no_evidence` として扱う（`invalid_evidence` を記録）。**すべての受入基準が `satisfied` で根拠が有効なときだけ `success`**。受入基準がない MR は `success`（件数 0）。
- **入力の扱い**: プロンプトで、kanflow から来た受入基準と差分の文を「信頼しない入力」として区切る。差分に書かれた文（「すべて満たしている」、`// satisfies AC-3`）は判定の根拠にしない。
- 入力は §2.3 と同じく、仕様のボット（§4.1.3）の主体で `prove(RequirementRead / MergeRequestRead / RepoRead / PipelineRead)` を通してから読む（読める範囲は §4.0 のローダーの規則）。要件の本文は summary に入れず、
  画面で `RequirementRead` を満たす人にだけ表示する。結果を書く `InternalCheckWrite` は `name = forge/spec-conformance` だけ。
- **`error` の種類**（チェックの結果と監査に残す）: `unavailable`（ゲートウェイのタイムアウト・接続の失敗、遮断器の「停止」、インスタンスの縮退）と、
  `invalid_output`（ゲートウェイが健全なのに、出力がスキーマの再試行の後も不正）。`invalid_output` は入力を受入基準ごとに分けて 1 回だけやり直し、それでも不正なら `error`（`invalid_output`）のまま残す。
- **段階（必須化の時期、2026-10-28 ユーザー確認）**:
  1. **影の段階**（M5 の導入から、提案 8 週間）: 助言のチェックとして全プロジェクトで動かし、偽の `failure` の率（事後のレビューで「誤り」とされた `failure`）、`error` の率（種類ごと）、待ち時間を測る。
  2. **必須化**: 基準（提案: 偽の `failure` 5 % 未満、`error` 2 % 未満、p95 の待ち 15 分未満）を満たしたら、全プロジェクトで必須にする（設定で外せない）。満たさなければ ADR で見直す。
- **可用性の `error` のときだけの免除**（必須化の後。`failure` と `invalid_output` は免除しない）:
  - 条件: 同じ head の SHA で②が `unavailable` の `error` のまま 30 分以上、またはゲートウェイの遮断器が「停止」を報告している、またはインスタンスが縮退（`SpecConformanceDegradedSet`）にある。
  - 免除できる人: `native_role` で Maintainer 以上の人間で、作成者・コミッター・enabler でない。kanflow の出どころの能力、代理、ボット、トークンは deny（`SessionOnly`）。
  - 記録: head の SHA に結びつけ、理由（必須の文）を付けて、チェックの結論を `neutral`（`waived`）にする。監査、イベント `check.waived`、登録したコード `SpecConformanceWaived`（warn）を出す。
  - それでも残るもの: ①は必須のまま。免除した②の MR は③の対象外。**必要な人間の承認が 1 件増える**（blocker `extra_human_approval_required`）。
  - **後からの確認**: ゲートウェイの回復後、マージ済みの免除した MR に②をもう一度実行する。`failure` なら alert `SpecConformanceRetroFailed` を出し、フォローアップの作業項目を作り、doctor の指標に数える。
  - `invalid_output` が続く MR は、Runbook（`spec-conformance-unavailable.md` の「出力の不正」）でプロンプト・モデル・入力の大きさを調べる。免除の経路はない（健全なゲートウェイでの不正は品質の問題で、可用性の問題ではない）。
- **飢餓を防ぐ**: §3 の MR レビューの層の最低の GPU 時間の予約と、待ちの昇格。

##### 4.1.3 ③ AI（仕様照合）の承認 — プロジェクトが選んだカテゴリだけ

- **主体**: ボット `forge-spec-lane`（`Kind::Bot(BotKind::SpecLane)`、資格情報なし、ユーザー名と email は予約、worker だけが作る）。`BotKind::allows()` の `SpecLane` は
  `MergeRequestAiApprove`、`InternalCheckWrite`（`forge/spec-conformance` だけ）、`MergeRequestRead`、`RepoRead`、`PipelineRead`、`RequirementRead` だけ。読める範囲は §4.0 のローダーの規則（MR のプロジェクトと固定した仕様のプロジェクト、読み取りだけ）。
  **人間の `MergeRequestApprove` はこのボットにも deny**、`MergeRequestAiApprove` はこのボット以外（人間・管理者・自動修正のボット・サービスアカウント・代理・システム）にすべて deny（生成した行）。
- **順序（認可を先に）**: worker は `check.completed` を購読し、MR の行をロック → **spec-lane の主体で `prove(MergeRequestRead)`・`prove(RepoRead)`・`prove(PipelineRead)`** → それから git を読んで `classify_ai_approval()` →
  分類の結果を入力にした `AiApprovalTarget` で `prove(MergeRequestAiApprove)` → 承認の行と根拠の記録を 1 つの ChangeTx で書く。
- **設定（2 つとも有効なときだけ）**: DB のプロジェクトの設定（`AiApprovalSettingsManage`、`native_role` で Maintainer 以上）と、対象ブランチの `.forge/repo.yml` の `review.ai_approval.categories`。実効 = 両者の共通部分（既定は空 = 無効）。
  **MR の tree の版は読まない**。`.forge/**` を変える MR は除外で対象外で、さらに §2.7 の blocker で `native_role` の Maintainer の承認が要る。インスタンスの停止（`AiApprovalPause`）は止めるだけで、緩められない。
- **カテゴリ**（MR 全体が 1 つのカテゴリに入るときだけ。混在は対象外。判定は merge-base..head の `diff-tree -r --no-renames --raw` と `--numstat` に対する純粋な関数 `classify_ai_approval()`、LLM は使わない。
  **追加・変更するファイルのモードは 100644 だけ**（新しいシンボリックリンク・gitlink・実行ファイルは対象外））:

| カテゴリ | 条件 |
|---|---|
| `docs_only` | 変えたパスがすべて対象ブランチの `review.ai_approval.docs_paths` に一致（**既定は `docs/**` だけ。`**/*.md` は既定にしない**）。次は対象外: §2.1 の固定の拒否、ADR（`**/adr/**`）、Runbook（`**/runbooks/**`）、エージェントの指示とプロンプトのファイル（`AGENTS.md`、`**/SKILL.md`、`.cursorrules`、`CLAUDE.md`、`.claude/**`、プロンプトの置き場所）、`.forge/repo.yml` の `specs:` の `path`、`spec` の種類のリポジトリ、**対象ブランチの tree で埋め込まれるファイル**。埋め込みの判定は **閉じた既定**: 登録した仕組み（Rust の `include_str!` / `include_bytes!` に **文字列の定数** を渡したもの、登録した文書のビルドの設定（mdBook など））だけを解析して対象外のファイルを計算し、**引数が定数でない埋め込み（`concat!`・`env!`・マクロ）、または登録していない埋め込みの仕組み（既知の検出の一覧: `//go:embed`、`build.rs` の存在、`include!`、登録していない文書のビルドの設定など）を 1 つでも検出したら、そのリポジトリでは `docs_only` を使えない** |
| `test_only_addition` | 変えたパスがすべて `review.ai_approval.test_paths`（既定 `tests/**`）に一致し、**削除の行・ファイルの削除がない**（追加だけ）。次は対象外: テストの支援のファイル（`conftest.py`、`**/tests/common/**`、`**/test_helper*`、`jest.setup.*`、`setupTests.*`、`**/fixtures/**`）、§2.2 のスキップの印、**`@req` の追加・変更**。さらに、マージ後のパイプラインのテストのジョブが **シークレットと環境を持たないこと** を、対象ブランチの `.forge-ci.yml`（ジョブの宣言）と **DB**（そのジョブに渡りうるプロジェクト・グループの CI の変数、保護ブランチに限った変数、結びついた環境）の両方で確かめる |
| `dependency_patch` | 変えたパスがロックファイル（`Cargo.lock`。cargo の形式の構文解析器で読む）だけ。変わったパッケージは名前と出どころ（内部のミラーのレジストリ）が同じで、版の変更が semver のパッチだけ（`0.y.z` は `z` だけ）。パッケージの追加・削除なし、git / path の出どころなし、yank されていない、**公開から 7 日以上**（ミラーが公開の時刻を持たなければ対象外）。マニフェストは変えない。`cargo audit` と `cargo deny check` の必須チェックが今の head で成功 |
| `deterministic_fix` | MR の tree が、対象ブランチの `autofix.fixers` の fmt / clippy `--fix` を merge-base に適用した結果と一致する（Runner のジョブで tree だけを作り、§1 ④と同じ経路で比べる。MR のパイプラインを動かした Runner とは別の Runner）。`.rs` などの登録した拡張子だけ |

- **除外**（カテゴリより先に判定し、設定で緩められない）: 保護パス（§2.7）、マイグレーション（登録したマイグレーションの置き場所）、
  CI の設定（`.forge-ci.yml`、`.forge/**`、対象ブランチの `.forge-ci.yml` が読み込む・実行するファイル）、§2.1 の固定の拒否。判定は §2.1 と同じ関数を使う。
- **自己承認・人の関わらない MR の禁止**: merge-base..head に **`autofix_commits`（DB の記録）にある AI のコミットが 1 つでもあれば対象外**（`ai_commit_present`。トレーラーは偽装されうるので DB を正とする、決定 6）。
  **MR の作成者か、コミットを push した主体に、エージェントと宣言されたトークンかサービスアカウントがいれば対象外**（`agent_involved`。決定 5 と同じ既定）。②を免除した MR も対象外（`spec_conformance_waived`）。
  `deterministic_fix` は人間が fmt / clippy を実行してコミットした MR だけが対象になる。
- **出す条件**: ①と②がともに今の head で `success`、他の必須チェックが `failure` / `error` でない、MR が open、停止が off、今の head にまだ `ai_spec` の承認がない。
  対象外なら承認を出さず、MR に「AI の承認の対象外: <理由>」を表示する（`AiApprovalNotEligible`、info）。
- **`evaluate_merge()` での数え方**: `ai_spec` の承認は **今の head と同じ SHA のものを最大 1 件**、必要な承認の数に数える。ただし、
  (a) **CODEOWNERS の承認にはならない**、(b) **保護ブランチの「人間の承認 1 件以上」（blocker `human_approval_required`）と `required_native_approvals` を満たさない**、(c) §4.0 の「本人の直接の資格情報」にならない、
  (d) §2.7 の `native_maintainer_approval_required` を満たさない、(e) 承認の行の **`policy_version`**（DB のカテゴリ、対象ブランチのファイルのカテゴリ、保護パスの集合、分類器の版のハッシュ）が今の値と同じで、停止が off のときだけ数える
  （保護パスが後から足されたら数えなくなり、worker が新しい方針で分類し直す）。
- **取り消し**: 新しいコミットで消える（SHA に結びつくため。`carried_from` で写さない）。新しい head でも条件を満たせば改めて出す。停止で既存の `ai_spec` の承認は数えなくなる。
- **表示**: 承認の一覧に種類「AI（仕様照合）」として人間の承認と区別して表示し、カテゴリ、①②の結果へのリンク、根拠の画面（`AiApprovalEvidenceRead`）を付ける。

##### 4.1.4 根拠と入力の保持

- `ai_approval_records`（追記のみ）: 承認の ID、MR、`sha`、カテゴリ、`policy_version`、`classify_ai_approval()` の入力と結果（パスの一覧と規則の版）、①②のチェックの実行の ID、
  ②の判定の JSON（受入基準ごとの判定と根拠の行）、モデル、プロンプトの版、入力の一覧（出どころの ID + ハッシュ。差分は SHA で、要件は仕様の版で再現できる）を **永久に** 持つ。
  入力の本文（組み立てたプロンプト）は暗号化して 1 年持つ（§4.1.6 の数か月の計測と事後のレビューに足りる期間）。読み取りは `AiApprovalEvidenceRead`（§2.3 と同じ形）。

##### 4.1.5 ADR-0003 と ADR-0004 の改訂（正確な文）

- **ADR-0003 Decision の 1 項目め**「既定は助言…プロジェクトの設定で必須にできる」に追記: 「例外: `forge/spec-conformance` は、影の段階の計測が基準を満たした後、全プロジェクトで必須チェックにする。
  必須化の後も、可用性の失敗の `error`（タイムアウト・遮断器の停止・縮退）のときだけ、条件付きの人間の免除（SHA に結びつけ、監査し、後からの確認をする）を設ける。出力の不正と `failure` は免除しない（ADR-0017 §4.1.2）」。
- **ADR-0003 Decision の 2 項目め**「LLM の判断だけでマージやデプロイを実行する経路は作らない」を次に改める:
  「LLM の判断だけでマージやデプロイを実行する経路は作らない。LLM の結果がマージ条件に入るのは、(a) 必須チェックの結論、(b) プロジェクトが DB と対象ブランチの両方で明示的に選んだカテゴリでの承認 1 件（AI（仕様照合）、ADR-0017 §4.1.3）の 2 つだけである。
  どちらの場合もマージには、決定的な必須チェックのすべての成功、保護ブランチでの人間の承認 1 件以上、`evaluate_merge()` の `Mergeable` と、人間の操作（直接・代理・予約）が要る。
  **エージェントと宣言されたトークンとサービスアカウントのトークンはマージも承認もできず**、すべてのマージはその出どころ（主体の種類・資格情報・経路）を記録する。
  LLM がマージを実行することはない（自動マージは保留、ADR-0017 §4.1.6）」。
  （「出力が操作を起こすことはない」の改訂は本 ADR の Decision の「方針」の文のとおり。AI の承認は操作ではなく、`evaluate_merge()` が数える記録である。）
- **ADR-0004 Consequences の Risks**「`forge/spec-trace` で検出する（助言から始め、計測後に必須化を判断）」を次に改める: 「`forge/spec-trace` を全プロジェクトの必須チェックにする（ADR-0017 §4.1.1、ユーザー決定 2026-10-28）。
  受入基準の照合 `forge/spec-conformance` は影の段階の後に必須チェックにする（ADR-0017 §4.1.2、ユーザー確認 2026-10-28）」。ADR-0004 の Decision に「要件に触れる MR は ①②のチェックを通る」の 1 行を足す。

##### 4.1.6 ④ 自動マージ（保留）と計測の基準

- **今は作らない**。`execute_merge()` を呼ぶのは人間の要求（直接・代理）と、人間が作った予約・キュー・MR セットの worker だけで、AI の承認を契機にマージする経路はない。
- **計測**: カテゴリごとの **誤った AI 承認の率** = AI の承認を数えてマージされた MR のうち、(a) 30 日以内に revert か「修正」の MR が結びついた、(b) 事後のレビュー（`AiApprovalFeedback`。作成者・コミッター・enabler は自分の MR に付けられない）で「誤り」、
  (c) インシデントの記録が原因として結びつけた、のいずれか ÷ AI の承認を数えてマージされた MR。事後のレビューは AI の承認でマージされた MR から週ごとに抜き取る（提案: 10 %、最低 5 件）。
- **再検討の開始条件**（提案。再検討の ADR で確定する）: 計測が 3 か月以上、カテゴリごとに 100 件以上、誤りの率の 95 % 信頼区間の上限が 2 % 未満。対象は **保護されていないブランチから**。

#### 4.2 kanflow からの承認・マージの確認の画面（決定 10 の A）

```
kanflow の画面で「承認」/「承認してマージを予約」/「予約」/「マージ」/「取り消し」を押す
  → kanflow のサーバーが 128 bit 以上の乱数の `state` を作り、そのときの kanflow のセッションの利用者と結びつけて保存し、ポップアップで forge の確認の画面を開く（GET /-/confirm/…?intent&sha&integration&state）
  → forge のセッション（なければ SSO / LDAP のログイン。戻り先は同じ origin のパスだけ）。MR が見えなければ 404
  → 画面は forge のデータだけを表示: MR の番号・タイトル・対象ブランチ、head の SHA（全桁）、変更の要約（ファイル数、+/-、チェックの結論、blockers、承認（種類別））、
    押す内容（intent と method）、**主張を受け取る連携の名前**。`integration` がこの MR のプロジェクトに結びついていない（`via_bound = false`）・`sha` が今の head と違う・本人に intent の権限がない → 押せない（理由は MR が見える人にだけ）
  → 本人が押す（POST …/assertions、CSRF）。`MergeAssertionIssue` の authorize が、同じセッションの主体で intent の Action の規則と `via_bound` を同じ Resource で判定する
  → forge が主張（不透明な 256 bit の乱数）を作り、ハッシュと結びつけ（下）を `merge_assertions` に保存。画面は「完了。閉じてください」だけを表示する（主張を画面に出さない）
  → kanflow のサーバーが、サービスアカウントのトークンで POST /integrations/{id}/merge_assertions/claim {state} を呼び、主張と `sub`（主張を押した forge の利用者の ID と変わらない ID）を受け取る（1 回限り、5 分以内）
  → **kanflow は `sub` が、その `state` に結びつけた kanflow のセッションの利用者と同じ人であることを確かめ、違えば捨てる**（別の人の forge のセッションで押された主張を使わない）
  → kanflow がサービスアカウントのトークン + `Forge-Act-As: user:<email>` + `Forge-Assertion: <主張>` で REST を呼ぶ
  → forge が検証（下）→ Principal::act_as(…, VerifiedAssertion) → MR の行をロック → prove → evaluate_merge()
  → 主張の消費: 成功は処理と同じ ChangeTx。検証の後に失敗（409・403 など）した場合も、別の確定したトランザクションで `rejected` として消費する（使い回せない。kanflow は画面を開き直す）
```

- **主張の形**: 署名した JWS ではなく **不透明な乱数**（forge が発行も検証もするので署名は要らない。アルゴリズム・`kid` の落とし穴と新しい crate を避ける）。
  `merge_assertions`（追記のみ + 消費の列）: `id`、`token_hash`、`integration_id`（`aud`）、`user_id`（`sub`）、`intent`、`route`（メソッド + パスの実体。intent ごとに 1 つのルート）、
  `body_digest`（正規化した本文 `{sha, method}` のハッシュ。MR セットは手順の一覧のハッシュ）、`state_hash`、`issued_at`、`expires_at`（≤ 5 分）、`claimed_at`、`consumed_at`、`outcome`。
- **intent とルートの対応（1 対 1）**: `approve` ↔ `POST …/approvals`、`unapprove` ↔ `DELETE …/approvals/me`、`merge` ↔ `POST …/merge`、`enqueue` ↔ `POST …/merge_queue`、`reserve` ↔ `POST …/merge_reservation`、
  `approve_and_reserve` ↔ `POST …/approve_and_reserve`、`cancel_reservation` ↔ `DELETE …/merge_reservation`、`merge_set` ↔ `POST …/merge_set_runs`。
- **検証**: 主張のハッシュが存在し、`integration_id` が要求を認証したサービスアカウントの連携と一致、`user_id` が `Forge-Act-As` で解決した本人と一致、**`intent` と `route` が今の要求のルートと一致**、
  **`body_digest` が今の本文の正規化のハッシュと一致**（代理では `commit_message` を受け付けない）、`sha` がロックの中で今の head と一致、期限内、受け取り済みで未消費。どれかが違えば 401 `ActAsAssertionInvalid`（`reason`）。代理の変更の操作に主張がなければ同じく拒否（`reason = missing`）。
- **なぜ「直接の資格情報」に数えるか**: 主張は **forge だけが、本人の forge のセッションから、forge が表示した MR・SHA・要約・受け取る連携を本人が見て押した操作でだけ** 発行する。kanflow は運ぶだけで作れず、
  1 つの主張は 1 つの intent・ルート・本文・SHA に 1 回だけ使える。したがって 1 つのサービスアカウントが本人のセッションなしに N 人分の承認を作れず、§4.0 の「保護ブランチでは本人の直接の資格情報から来ること」を満たす。
  承認の行に `direct = true`、`credential_origin = act_as_assertion` を記録する（セッション = `session`、個人のトークン = `token`）。
- **画面の守り（クリックの時機を狙う攻撃を含む）**:
  - ポップアップ（トップレベル）で開き、`Content-Security-Policy: frame-ancestors 'none'`、`X-Frame-Options: DENY`、`Cross-Origin-Opener-Policy: same-origin`（主張を `window.opener` に渡さないので opener は要らない）。
  - **押すボタンは、画面が表示されフォーカスを持ってから 1 秒以上たち、その後に画面の中で新しいポインターかキーの操作があるまで無効**。ボタンに既定のフォーカスを置かず、Enter で送信しない。
  - **保護ブランチへの `merge` / `reserve` / `approve_and_reserve` / `merge_set` は、MR の番号の入力を求める**。
  - `state` は 128 文字以内の `[A-Za-z0-9_-]` だけを受け、エスケープして扱う。発行は本人ごとに流量を制限する。
- **ログ**: 主張の本文は記録しない。アクセスログとトレースのミドルウェアで、ヘッダの `Forge-Assertion` と `Forge-Act-As`（email は個人情報）、**確認の画面の URL の `state` の値、claim の本文の `state`** を伏せる。
  記録するのは `assertion_id`、`intent`、`integration_id`、`mr_id`、`sha`、`state_hash` の先頭 8 文字。
- **kanflow の側の作業**（`02` に追記）: `state` を作って kanflow の利用者と結びつけ、確認の画面をポップアップで開く、サーバーから claim で主張と `sub` を受け取り、`sub` を照合してから `Forge-Assertion` で送る、
  `merge_reserved` / `reservation_cancelled` / `merged` / `merge_blocked` のイベントを受け取って PBI の画面に出す、`state` をログに残さない。

#### 4.3 マージの予約（決定 10 の D）

- **作成**: `POST …/merge_reservation`（`MergeReservationCreate`）か `POST …/approve_and_reserve`（`MergeRequestApproveAndReserve`、承認も同時に。主張は intent ごとに 1 つ）。1 つの ChangeTx で、MR の行のロック → prove → `sha` が今の head と一致 →
  （承認）→ `merge_reservations` に 1 行（`mr_id`、`reserved_by`、`via_id?`、`credential_id`、`credential_kind`、`act_as_claimed?`、`via_assertion_id?`、`sha`、`method`、`expires_at`、`state = active`）→ マージキューへの投入 → 監査 + `merge_request.merge_reserved`。
  MR あたり有効な予約は 1 つ（部分一意の索引）。forge の画面（セッション）からも同じルートで予約できる。`expires_at` = min（24 時間、元の資格情報の期限）。
- **実行**（worker、マージキューの CI が緑になったとき）: MR の行をロック → `Principal::reserved(本人, 予約)`（`Credential::Reserved`、`MergeRequestMerge` だけ）で **今のロール（`role_for(MergeRequestMerge)`、§4.4 の能力を含む）・本人が有効・
  元の資格情報が今も有効**（トークンなら失効・期限切れでなく `MergeExecute` を持つ、セッションなら失効させられていない、代理ならサービスアカウントのトークンが有効で `MergeExecute` を持ち `via_bound`）を確かめて
  `prove(MergeRequestMerge)` → `evaluate_merge()`（その時点の状態）→ **head の SHA = 予約の `sha`** を確かめる → `execute_merge()`。監査とイベントは予約の作成時の主体の記録（`actor_kind`、`credential_id`、`act_as_claimed`、`via_assertion_id`）を写し、`merge_origin` に `reservation_id` を付ける。
  予約が主張かセッションで作られていれば、そのマージは §4.0 の「本人の直接の資格情報から来るマージ」に数える。
- **取り消し**（`merge_request.reservation_cancelled`、`reason`）: 新しいコミット（`head_changed`。push と同じ ChangeTx で構造的に。自動修正はキューの MR に動かない）、本人・`native_role` の Maintainer の取り消し（`user_cancelled`）、
  **資格情報の失効**（`credential_revoked`: トークンの失効、セッションの失効、パスワードの再設定、サービスアカウントの失効。失効の ChangeTx で、その資格情報の予約をすべて取り消す。実行時の確認でも拒否する）、
  実行時の認可の失敗（`role_lost` / `user_inactive` / `scope_lost`。kanflow の出どころの能力が消えた・古くなった場合も `role_lost`）、`evaluate_merge()` が `Blocked`（`blocked`、`blockers` を付ける）、キューの CI の失敗（`pipeline_failed`）、MR のクローズ（`mr_closed`）、
  期限（`expired`、既定 24 時間、プロジェクトで下げられる）。黙って消さず、MR の画面とイベントで知らせる。代理での取り消しは主張（`cancel_reservation`）を要し、監査し、本人ごとに流量を制限する。
- 予約は自動マージではない: 予約を作るのは人間の操作で、マージの条件は他の経路と同じ `evaluate_merge()`。

#### 4.4 kanflow のオーナーの能力の同期（決定 11）

**判定点は 1 つのまま**: `authorize()` は forge の DB から読んだ `ProjectView` だけで判定し、**判定の時点で kanflow を呼ばない**。kanflow のオーナーは、同期の worker が forge の DB に **能力の付与** として実体化する。

- **与えるのはロールではなく能力の許可リスト**: `Action::kanflow_grant_allowed()`（`_` のない全列挙）が true の Action だけで、kanflow の出どころの付与が Maintainer として数えられる:
  `ProjectRead`、`RepoRead`、`MergeRequestRead`、`MergeSetRunRead`、`AutofixAttemptRead`、`MergeRequestApprove`、`MergeRequestUnapprove`、`MergeRequestMerge`、`MergeQueueEnqueue`、`MergeReservationCreate`、
  `MergeRequestApproveAndReserve`、`MergeReservationCancel`、`MergeSetRunCreate`、`MergeConfirmView`、`MergeAssertionIssue`。
  **それ以外のすべての Action は `native_role()` で判定する**（`RepoPush`、`IntegrationBindingManage` を含む。**直接の push は与えない**）。今後足す Action も、許可リストに明示しない限り kanflow の出どころでは許されない（既定で閉じる）。
  マトリクスの行は `for a in Action::ALL` × 「kanflow の出どころだけの Maintainer」で生成し、許可リストの外は deny を確かめる。
- **表**: kanflow の出どころの付与は、forge の所属の表とは **別の表** `kanflow_grants`（`project_id`、`user_id`、`binding_id`、`kanflow_project_id`、`granted_at`。役割は固定で列を持たない）に追記のみのマイグレーションで作る。
  同期の worker が書くのはこの表と同期の状態の表だけで、**forge の所属（`project_members` / グループの所属）を変えることも消すこともない**。グループ・Owner・管理者は作れない（表と型に該当する列も値もない）。
- **鮮度はロールを読む SQL の中で判定する**（ジョブに依存しない）: 同期の状態 `membership_sync_state`（連携 × kanflow のプロジェクトごとに 1 行。`last_success_at`）を持ち、ローダーは次をすべて満たす付与だけを数える。
  `kanflow_grants` の行がある ∧ 結びつけ（`binding_id`）が存在し、同期が有効で、`kanflow_project_id` が付与と同じ ∧ 連携が有効で、サービスアカウントが有効 ∧ インスタンスの同期の停止が off ∧
  `membership_sync_state.last_success_at > now() - suspend_after`（既定 4 時間）。成功した同期は、そのときのオーナーでない人の行を消し、`last_success_at` を進める（鮮度は kanflow のプロジェクトごと）。
  したがって kanflow・worker・ジョブのどれが止まっても、4 時間で付与は数えられなくなる（与えない側に倒れる）。
- **同期の流れ**（ジョブ `kanflow.membership_sync`、`forge-integ`、主体 `Principal::system(MembershipSync)`。kanflow のプロジェクトごとに同時に 1 つ）:
  1. 契機: **5 分ごと** + kanflow からの通知 `project.members_updated`（`02` §5.6 に追加。kanflow のプロジェクトごとに 1 分に 1 回へまとめる。通知の本文はデータとして使わない）。
  2. `GET /projects/{project_id}`（`02` §4.2、メンバーとロール）を、TLS の検証あり・タイムアウト 10 秒・応答の大きさの上限・ページの上限付きで取得。形が不正、または一部しか取れなければ **失敗として何も変えない**。
  3. role = owner の人を、**変わらない ID（LDAP の ID / `employeeNumber`）を必須のキー** として forge のユーザーに突き合わせる。email は確認だけに使い、ID と email が食い違えば `ambiguous`。
     見つからない・無効・人間でない・あいまいは飛ばして記録する（`KanflowMemberUnmatched`）。
  4. 純粋な関数 `plan_membership_sync()` が、望む集合と今の `kanflow_grants` の差分を計算する。
  5. **追加の上限**: 1 回の同期で 1 つの forge のプロジェクトに 5 人、かつ **直近 24 時間で 10 人** を超える追加は適用せず保留し alert（`KanflowMembershipBulkHeld`）。
     保留は `native_role` の Owner が `IntegrationMembershipSyncAcknowledge`（セッションだけ）で一覧を見て確認するまで適用しない。**削除は保留中も上限なく適用する**。
     **取得と削除が成功し、追加だけを保留した実行は「成功」として数え、`last_success_at` を進める**（保留が既存の付与の鮮度を落とさない。保留の件数は同期の状態と doctor で見える）。
  6. `prove(MembershipSyncApply)`（`Resource::KanflowGrantSet { binding, sync_enabled, via_bound }`）→ forge のプロジェクトごとに 1 つの ChangeTx で適用し、追加・削除ごとに監査の行 `membership.synced_from_kanflow`
     （`actor_kind = system`、`details: {change: added | removed, kanflow_project_id, binding_id, forge_project_id, sync_run_id}`）とイベント `project.membership_synced`（email は入れない）。`last_success_at` の更新は同じ ChangeTx で、`membership.sync_confirmed`（結びつけごとに 1 行）を監査に残す。
  7. **追加の通知**: 付与を 1 件でも追加したら、その forge のプロジェクトの `native_role` の Owner 全員に通知する（画面の通知と、設定した場合はメール。誰に・どの kanflow のプロジェクトから・いつ付いたか、取り消しの手順へのリンク）。
     **残る危険**: 通知は検出であって防止ではない。上限の範囲（1 回 5 人・24 時間 10 人）の追加は、Owner が気づくまで能力を持つ。ただし能力はマージ・承認・キュー・予約・読み取りに限られ、
     保護ブランチの規則・他の人の承認・§2.7 の `required_native_approvals` と `native_maintainer_approval_required`・（kanflow からは）本人の forge のセッションの主張が残る。
- **観察のジョブ**（権限の判定には使わない）: 1 分ごとに、`last_success_at` が `suspend_after` を超えた kanflow のプロジェクトを見つけ、`prove(MembershipSyncSuspend)` で監査 `membership.sync_suspended` / `resumed`、
  イベントと alert `KanflowMembershipSuspended` を出す。このジョブが止まっても、数えられなくなることは SQL が保証する（記録だけが遅れる）。
- **有効化と無効化**: 同期の有効化（`IntegrationMembershipSyncToggle`）は **`native_role` で Owner だけ、セッションだけ、代理は不可**。有効化の画面で kanflow の今のオーナーの一覧と、同じ forge のプロジェクトに付く付与の合計
  （`spec` リポジトリが複数の kanflow のプロジェクトに結びつく場合は、すべての結びつけの合計）を見せ、本文にその一覧のハッシュを入れて押す（最初の付与の確認）。
  有効化は（`binding_id`、`kanflow_project_id`）に結びつけ、結びつけの先を変えたら自動で無効にする。**無効化・結びつけの解除（`IntegrationBindingManage`）は同じ ChangeTx でその結びつけの `kanflow_grants` を消す**（監査とイベント付き）。
- **取り消しの遅れの上限**: kanflow でオーナーでなくなった人の付与は、kanflow に届く間は **次に成功した同期で消える**（通知で通常は数秒、通知がなくても最大 10 分 = 5 分の間隔 + 実行の上限 5 分）。
  **kanflow に届かない間は最大 4 時間** で数えられなくなる。消えた・数えられなくなった時点で、その人の予約は実行時の再認可で `role_lost` になる（§4.3）。
- **4 時間を選んだ理由**: 「両方がマージできる」ので、kanflow の出どころの能力を止めても forge の Maintainer はマージを続けられ、止めたときの損失は小さい。一方、取り消しが届かないまま能力が残る時間は短いほど良い。
  4 時間は kanflow の通常の再起動・夜間の保守を越えて能力を揺らさず、長い障害では当日のうちに止まる長さとする（設定で下げられる。上げるのは管理者）。
- **マージの規則は変わらない**: 保護ブランチの規則、必須チェック、承認（自己承認の禁止、SHA に結びついた承認、CODEOWNERS（ロールの指定は `native_role`）、§2.7）、§4.2 の主張は、ロールの出どころに関係なくそのまま適用する。
  監査とイベントのマージの行に `role_source`（`forge` / `kanflow`、同じ値なら `forge`）を残す。

#### 4.5 Action ごとの allow / deny の行（`forge-auth/src/matrix_rows.rs` の最小の行）

既存の Action の名前は `action.rs` のとおり（`RepoRead`、`RepoPush`、`ProjectRead`、`ProtectedRefManage`、`MemberManage`、`TokenCreate` など）。下の表は新しい Action で、どれにも共通の deny の行として
**生成した行**（ボット・システム・`Reserved`・`Queued`・`JobToken`・`ActAs`・kanflow の出どころだけの主体 × 許可リストの外、`SessionOnly` × セッション以外、エージェントと宣言されたトークン・サービスアカウント × `is_merge_family()` と `MergeRequestApprove`、
`required_scopes()` の `All` の一部だけを持つトークン）を持つ。
`synced`・停止した・消えた付与の区別は `authorize()` には見えない（ロールなしと同じ）ので、**forge-db のローダーのテスト**（`kanflow_grants` × 結びつけ・連携・鮮度・停止の各状態 → `role_for()` の結果、spec-lane の読める範囲）で確かめ、マトリクスは「ロールなし → deny」の行で受ける。

| Action | allow（最小の行） | deny（代表の行） |
|---|---|---|
| `MergeRequestRead` | Reporter（セッション）、kanflow の出どころ | 見えないプロジェクト → 404、匿名 |
| `MergeRequestApprove` | Developer（作成者でない）、代理 + `approve` の主張、kanflow の出どころ | 作成者（self_approval）、Reporter、`Api` だけのトークン、エージェントと宣言されたトークン、サービスアカウント、代理で主張なし・別の intent、ボット（生成） |
| `MergeRequestUnapprove` | 承認した本人、代理 + `unapprove` | 他人の承認、代理で主張なし |
| `MergeRequestMerge` | 保護の規則が許すロール（Maintainer など）、kanflow の出どころ、`Reserved` / `Queued`（今のロールが通る） | Developer（保護が Maintainer を要する）、Owner レベルの保護への Maintainer、エージェントと宣言されたトークン、サービスアカウント、代理で主張なし、`Api` だけ |
| `MergeQueueEnqueue` | `MergeRequestMerge` と同じ | 同上 |
| `MergeReservationCreate` | `MergeRequestMerge` と同じ（セッション・トークン・代理 + `reserve`） | 同上、`Reserved` / `Queued` |
| `MergeRequestApproveAndReserve` | 作成者でない Developer 以上でマージもできる人、代理 + `approve_and_reserve`、`MergeApprove` と `MergeExecute` の両方を持つトークン | 作成者（承認の側で deny → 全体 deny）、マージのできない Developer、片方のスコープだけのトークン、エージェント・サービスアカウント |
| `MergeReservationCancel` | 予約した本人、`native_role` の Maintainer、代理 + `cancel_reservation` | 他人の予約の Developer、kanflow の出どころだけで他人の予約、代理で主張なし |
| `MergeSetRunCreate` | 全手順で merge できる人、代理 + `merge_set` | 1 手順でも見えない → 404、1 手順でも merge できない、エージェント・サービスアカウント |
| `MergeSetRunRead` | 全手順のプロジェクトを読める人 | 1 つでも読めない → 404 |
| `MergeConfirmView` / `MergeAssertionIssue` | セッションで intent の Action が通る人（kanflow の出どころを含む） | トークン・代理（`SessionOnly`）、`via_bound = false`、intent の Action が deny、見えない MR → 404 |
| `MergeAssertionClaim` | 主張の `integration_id` の連携のサービスアカウント（`MergeApprove` か `MergeExecute`） | 別の連携、代理、人間のトークン、受け取り済み・期限切れ |
| `MergeRequestAiApprove`（INTERNAL） | `Bot(SpecLane)` + 対象のカテゴリ + `policy_version` が今の値 | 人間・管理者・`Bot(Autofix)`・サービスアカウント・代理（生成）、対象外の理由のそれぞれ |
| `InternalCheckWrite`（INTERNAL） | `System(SpecTrace)` × `forge/spec-trace`、`Bot(SpecLane)` × `forge/spec-conformance` | 名前の入れ替え、`Bot(Autofix)`、人間（生成） |
| `CheckRunWrite`（PLANNED、Checks API） | 結びつけた連携のサービスアカウント × 自分の名前空間のチェック（`kanflow/*`） | `forge/*` の名前、結びついていないプロジェクト、人間のトークン |
| `JobLogRead` / `PipelineRead`（PLANNED） | Reporter、`Bot(Autofix)` / `Bot(SpecLane)`（作成者 ∩ enabler / ローダーの範囲） | Guest、見えないプロジェクト → 404、匿名 |
| `RequirementRead`（PLANNED） | 仕様のプロジェクトの Reporter、`Bot(SpecLane)`（固定した仕様のプロジェクトだけ） | その仕様のプロジェクトを読めない人、固定されていない仕様のプロジェクトへの `Bot(SpecLane)` → 404 |
| `AiApprovalEvidenceRead` | 出どころのすべてを読める人 | 仕様のプロジェクトを読めない Reporter、別の MR の `approval_id` → 404 |
| `AiApprovalFeedback` | `native_role` の Developer（作成者・コミッター・enabler でない） | 作成者・コミッター・enabler、kanflow の出どころだけ、代理 |
| `AiApprovalSettingsManage` / `ProtectedPathManage` | `native_role` の Maintainer（保護パスの削除は Owner） | kanflow の出どころだけ、Developer、代理、保護パスを削除する Maintainer |
| `IntegrationBindingManage`（PLANNED） | `native_role` の Maintainer（同期が有効な結びつけの変更・解除は Owner）、セッション | kanflow の出どころだけ、代理、トークン、Developer、同期が有効な結びつけを解除する Maintainer |
| `AiApprovalPause` / `AiApprovalResume`、`AutofixPause` / `AutofixResume`、`IntegrationMembershipSyncPause` / `Resume`、`SpecConformanceDegradedSet` | 管理者 + `admin` スコープ | `admin` スコープのない管理者のトークン、管理者でない |
| `SpecConformanceWaive` | `native_role` の Maintainer（作成者・コミッター・enabler でない）、セッション、②が `unavailable` の `error` の条件を満たす | `failure` の②、`invalid_output` の②、条件の前、kanflow の出どころだけ、代理・トークン・ボット |
| `AutofixPush` / `AutofixCandidateWrite` / `AutofixComment`（INTERNAL） | `Bot(Autofix)` + `AutofixTarget` が通る | 人間・管理者（生成）、保護ブランチ、作成者・enabler がロールを失った、別のリポジトリのソース |
| `AutofixAttemptRead` / `AutofixAttemptInputRead` | Reporter / 出どころのすべてを読める人 | 見えない MR、出どころの 1 つを読めない、別の MR の `aid` → 404 |
| `AutofixSettingsManage` / `MergeRequestAutofixToggle` | `native_role` の Maintainer / 作成者・他人の MR の有効化は `native_role` の Maintainer | kanflow の出どころだけ、他人の MR を有効にする Developer、代理 |
| `AutofixCommitRevert` | `RepoPush` が通る人（`native_role`） | 保護 ref、kanflow の出どころだけ、MR の `autofix_commits` にない `sha` → 404 |
| `IntegrationMembershipSyncToggle` / `Acknowledge` | `native_role` の Owner、セッション | `native_role` の Maintainer、kanflow の出どころ、代理、トークン |
| `IntegrationMembershipSyncRead` | `native_role` の Maintainer | Developer、kanflow の出どころだけ |
| `MembershipSyncApply` / `MembershipSyncSuspend`（INTERNAL） | `System(MembershipSync)` + 有効な結びつけ | 人間・ボット・サービスアカウント（生成）、無効な結びつけ |
| （既存）`RepoPush`、`ProtectedRefManage`、`MemberManage`、`ProjectUpdate` ほか | `native_role` のとおり | **kanflow の出どころだけの Maintainer**（生成）、`MembershipChange.from` を kanflow の値で読む誤り |

### 5. 観測とエラー契約

- **イベント**（集約 `merge_request`、ADR-0016 の `integration` の分類）: `merge_request.autofix.toggled`、`.started`、`.applied`、`.discarded`、`.proposed`、`.gave_up`、`.reverted`、
  **`merge_request.merge_reserved`**（`reservation_id`、`sha`、`method`、`reserved_by`、`via`）、**`merge_request.reservation_cancelled`**（`reservation_id`、`reason`、`blockers?`）、
  `merge_request.approved` / `unapproved` に `approval_kind`（`human` / `ai_spec`）と `ai_category?`、`merged` に `merge_origin`、`check.waived`。
  集約 `project`: **`project.membership_synced`**（§4.4）。本文・差分・ログ・主張・`state`・email は入れない。
- **監査**: ボットの操作は `actor_kind = service_account` で、作成者・enabler・試行は `AuditDetails::Autofix`。`autofix.settings_changed`、`autofix.toggled`、`autofix.candidate_written`、
  `autofix.commit_pushed`、`autofix.patch_rejected`（denied）、`autofix.commit_reverted`、`autofix.paused` / `resumed`、`merge_request.approved`、`merged`（`merge_origin`）、`merge_denied`、`merge_set_run.created`、
  **`merge_request.ai_approved`**（`AuditDetails::AiApproval`: カテゴリ、`policy_version`、`ai_approval_records` の ID）、`ai_approval.settings_changed`、`ai_approval.paused` / `resumed`、`ai_approval.feedback`、`protected_paths.changed`、`protected_branch.native_approvals_changed`、
  **`check.spec_conformance_waived`**、`spec_conformance.degraded_set`、
  **`merge_assertion.issued`** / `claimed` / `consumed` / `rejected`（`reason`）、**`merge_request.merge_reserved`** / `reservation_cancelled`、代理の `unapproved` / 予約の取り消し、
  **`membership.synced_from_kanflow`**（`actor_kind = system`）、`membership.sync_confirmed`、`membership.sync_suspended` / `resumed`、`integration.binding_changed`、`integration.membership_sync_toggled`、`_acknowledged`、`_paused` / `_resumed`、`_held`、`_owner_notified`。
- **保持**: `autofix_attempts` は置換の JSON、入力の一覧（出どころの ID + ハッシュ）、enabler、モデル、プロンプトの版を **永久に** 持つ。入力と出力の本文だけを 30 日で消す。
  `ai_approval_records` は §4.1.4。`merge_assertions` は結びつけと時刻と結果を永久に（主張と `state` の値は持たず、ハッシュだけ）。`membership_sync_runs`（実行ごとの結果、件数、失敗の理由、kanflow の応答のハッシュ）を 1 年。
- **ログ**: 自動修正のすべての段階に `request_id`、`attempt_id`、`mr_id`、`trigger_kind`、`trigger_key`、`state`、`fixer`、`model`、`prompt_version`、`input_hash`。
  マージに `request_id`、`mr_id`、`merge_origin`（`principal_kind`、`credential_kind`、`agent_declared`、`via`、`reservation_id`、`queue_run_id`、`role_source`、`credential_origin`）、`head_sha`、`blockers`、`idempotency_key_hash`、`merge_set_run_id`、`assertion_id`。
  仕様照合に `check`（spec-trace / spec-conformance）、`phase`（shadow / required）、`head_sha`、`requirements`、`criteria_verdicts`、`invalid_evidence`、`error_kind`（unavailable / invalid_output）、`waived`、`model`、`prompt_version`、`input_hash`。
  AI の承認に `approval_id`、`ai_category`、`eligibility`（`reasons`）、`policy_version`、`classifier_version`。
  所属の同期に `sync_run_id`、`integration_id`、`kanflow_project_id`、`binding_id`、`added`、`removed`、`unmatched`、`held`、`last_success_at`、`error_code`（email は出さず、ユーザーの ID だけ）。
- **指標**: `forge_autofix_attempts_total{trigger,fixer,outcome}`、`_commits_landed_total`、`_commits_reverted_total`、`_patch_rejected_total{reason}`、`_time_to_green_seconds`、
  `forge_llm_queue_wait_seconds{priority}`、`forge_merges_total{via,method,outcome,role_source,principal_kind}`、`forge_merge_blocked_total{reason}`、
  `forge_spec_check_total{check,phase,conclusion,error_kind}`、`forge_spec_conformance_waived_total`、`forge_spec_conformance_retro_failed_total`、`forge_ai_approvals_total{category,outcome}`、`forge_ai_approval_false_total{category,signal}`、
  `forge_merge_assertions_total{intent,outcome}`、`forge_merge_reservations_total{outcome,reason}`、`forge_merge_reservation_wait_seconds`、
  `forge_kanflow_membership_sync_total{outcome}`、`forge_kanflow_membership_sync_age_seconds{kanflow_project}`、`forge_kanflow_membership_changes_total{change}`、`forge_kanflow_membership_unmatched`。
  `forge doctor`: `autofix.bot_account`（資格情報がないこと）、`autofix.stuck_attempts`、`autofix.orphan_candidate_refs`、`autofix.revert_rate`（7 日で 20 % 超で warn）、`merge_set_runs.stuck`、`uploadpack.hideRefs` の設定、
  `spec_lane.bot_account`、`ai_approval.false_rate`（カテゴリごと）、`spec_conformance.error_rate`（種類ごと）、`spec_conformance.waivers`（7 日の件数）、`spec_conformance.retro_failed`（件数 > 0 で warn）、
  `merge_reservations.stuck`、`merge_assertions.unconsumed`（受け取られず期限切れの率）、
  **`integration.membership_sync_age`**（最後の成功から 15 分超で warn、4 時間超（= 数えない）で error）、`integration.membership_sync_held`（> 0 で warn）、`integration.membership_unmatched`。
- **エラーコード**（実装時にテストと同じコミットで登録、番号は採番時。`INT`・`LLM`・`REVIEW` の `AREAS` への追加はエージェントモデル側が先）:

| 仮の名前 | 領域 | 重大度 | HTTP | 文脈の項目 |
|---|---|---|---|---|
| AutofixPatchRejected | LLM | warn | — | `attempt_id`、`reason`（forbidden_path / outside_scope / too_large / weakens_tests / delete_or_empty / bidi / mode / ambiguous_match / secret_in_output）、`path` |
| AutofixGaveUp | LLM | info | — | `reason`、`attempts`、`last_fingerprints` |
| AutofixFixerFailed / AutofixApplyFailed | CI | warn | — | `fixer` / `path`、`exit_code`、`job_id` |
| AutofixTestsWeakened | CI | warn | — | `job`、`tests_before`、`tests_after`、`missing_ids` |
| AutofixCandidateInvalid | GIT | **alert** | — | fsck の失敗、前提の不一致、範囲外、衝突の hunk の外の変更（Runner の不具合か侵害）。`attempt_id`、`runner_id`、`reason` |
| AutofixPushDenied | AUTH | **alert** | — | ④の先の `prove` か ⑦のロックの中の確かめ直しが拒否。`attempt_id`、`stage`、`ref`、`reason` |
| AutofixLandingRaced | GIT | info | — | `expected_sha`、`actual_sha` |
| BotIdentityPushRejected | GIT | warn | 403 | 人間の push にボット（`forge-autofix` / `forge-spec-lane`）の ID かトレーラー。`pusher_id`、`commit` |
| MergeBlocked / MergeHeadMismatch | REVIEW | info | 409 | `blockers`（`human_approval_required`、`native_maintainer_approval_required`、`native_approval_required`、`extra_human_approval_required`、`required_check_missing` を追加）/ `expected_sha`、`actual_sha` |
| MergeExecutionFailed | GIT | **alert** | 500 | `mr_id`、`method`、`merge_origin`、対象ブランチの比較して交換の失敗か git の終了コード（stderr はログだけ） |
| IdempotencyKeyConflict | WEB | info | 422 | 同じキーで違う本文 |
| ActAsScopeMissing / ActAsAssertionInvalid | AUTH | info / warn | 403 / 401 | スコープがない / 主張の `reason`（missing / unknown / wrong_integration / subject_mismatch / intent_mismatch / route_mismatch / body_mismatch / sha_mismatch / expired / not_claimed / replayed）、`assertion_id` |
| DirectCredentialRequired | REVIEW | info | 409 | 保護ブランチで本人の直接の承認もマージもない（§4.2 の主張でも満たせる） |
| MergeSetStopped | INT | warn | — | `merge_set_run_id`、`step`、`reason`、`merged_steps` |
| SpecTraceFailed | REVIEW（FG-REVIEW-next） | info | — | `mr_id`、`head_sha`、`untested: [req_id]`、`failing: [{req_id, test_id}]`。blocker は `MergeBlocked` |
| SpecConformanceUnsatisfied | REVIEW（FG-REVIEW-next） | info | — | `mr_id`、`head_sha`、`phase`、`criteria: [{criterion_id, verdict}]`、`invalid_evidence` |
| SpecConformanceEvidenceInvalid | LLM（FG-LLM-next） | warn | — | 引用した行が実在しない・コメントか文字列だけ（`no_evidence` として扱う）。`criterion_id`、`path`、`lines`、`reason`、`model`、`prompt_version` |
| SpecConformanceInvalidOutput | LLM（FG-LLM-next） | warn | — | 健全なゲートウェイで、分けてやり直した後も出力がスキーマに合わない（免除の対象外）。`mr_id`、`head_sha`、`model`、`prompt_version`、`input_tokens`、`attempts` |
| SpecConformanceWaived | REVIEW（FG-REVIEW-next） | warn | — | `mr_id`、`head_sha`、`waived_by`、`error_since`、`trigger`（timeout / breaker_open / degraded） |
| SpecConformanceRetroFailed | REVIEW（FG-REVIEW-next） | **alert** | — | 免除してマージした MR の後からの②が `failure`。`mr_id`、`merged_sha`、`criteria`、`follow_up_ref` |
| AiApprovalNotEligible | REVIEW（FG-REVIEW-next） | info | — | `mr_id`、`head_sha`、`reasons`（category_not_enabled / mixed_categories / protected_path / migration / ci_config / fixed_deny / mode / ai_commit_present / agent_involved / spec_conformance_waived / checks_not_green / paused / spec_path / include_target / unregistered_embed / test_support / req_tag_changed / secrets_in_post_merge / supply_chain）、`paths` |
| AiApprovalDenied | AUTH（FG-AUTH-next） | **alert** | — | `classify_ai_approval()` が対象としたのに `authorize()` かロックの中の確かめ直しが拒否（不具合か改ざん）。`mr_id`、`approval_id?`、`reason` |
| MergeReservationConflict | REVIEW（FG-REVIEW-next） | info | 409 | 既に有効な予約がある / キューの対象でない。`mr_id`、`existing_reservation_id` |
| MergeReservationCancelled | REVIEW（FG-REVIEW-next） | info | — | `reservation_id`、`reason`（head_changed / user_cancelled / credential_revoked / role_lost / user_inactive / scope_lost / blocked / pipeline_failed / mr_closed / expired）、`blockers?` |
| ConfirmIntegrationInvalid | INT（FG-INT-next） | warn | 400 | 確認の画面の `integration` が未登録・無効・この MR のプロジェクトに結びついていない、`state` がない・形が不正。`integration_id`、`reason` |
| MergeAssertionIssueFailed | INT（FG-INT-next） | **alert** | 500 | 主張の記録の書き込みに失敗。`reason`（DB の詳細はログだけ） |
| MergeAssertionClaimInvalid | INT（FG-INT-next） | info | 404 | claim の `state` が不明・受け取り済み・期限切れ・別の連携。`integration_id`、`reason` |
| KanflowMembershipSyncFailed | INT（FG-INT-next） | warn | — | 同期の 1 回の失敗（何も変えない）。`sync_run_id`、`integration_id`、`kanflow_project_id`、`reason`（unreachable / timeout / tls / auth_rejected / malformed / partial / too_large）、`http_status?`、`last_success_at`、`cause_code`（`FG-INT-0001` / `0002` があればそれ） |
| KanflowMembershipSuspended | INT（FG-INT-next） | **alert** | — | 最後の成功から `suspend_after` を超え、付与が数えられなくなった（観察のジョブが記録）。`kanflow_project_id`、`grant_count`、`last_success_at` |
| KanflowMembershipBulkHeld | INT（FG-INT-next） | **alert** | — | 1 回 5 人か 24 時間 10 人の上限を超え、追加を保留した（削除は適用、実行は成功として数える）。`sync_run_id`、`forge_project_id`、`requested_additions`、`window_additions`、`limit` |
| KanflowMemberUnmatched | INT（FG-INT-next） | info | — | オーナーを forge のユーザーに突き合わせられない。`kanflow_project_id`、`reason`（unknown / disabled / not_human / ambiguous）、`subject_hash`（ID も email も出さない） |
| KanflowGrantNotificationFailed | INT（FG-INT-next） | warn | — | 付与の追加の Owner への通知に失敗（付与は取り消さない。画面の通知は ChangeTx で作り、メールだけが失敗しうる）。`forge_project_id`、`grant_count`、`channel` |

LLM の応答なし・出力の不正（自動修正の側）は ADR-0003 のゲートウェイのコードを、代理の対象の不正は `FG-INT-0006` を使い回す。確認の画面・主張の発行・kanflow の出どころの能力の外・エージェントと宣言されたトークンのマージの拒否は `authorize()` の共通の拒否のコードを使う（`reason` に `kanflow_grant_not_allowed`、`non_human_merge`、`session_required`、`scope_requirement`）。

- **品質の輪**（`01` §6.5）: revert・「誤り」と評価された AI コミット、AI コミットの後にクローズされた MR、`AutofixPatchRejected`、**「誤り」と評価された AI の承認と②の判定、免除した MR の後からの `failure`、②の `invalid_output`** を評価セットの候補に入れ、人が選んで加える
  （本文はオンプレミスの実行時データ。`Git/` に入れるのは合成か匿名化したものだけ、CLAUDE.md §3-8）。指標は trigger ごとの pass@1、有害率、revert 率、GPU 時間、カテゴリごとの誤った AI 承認の率、②の偽の `failure` の率。

### 6. 段階

| 段階 | 範囲 |
|---|---|
| **M3** | `evaluate_merge()`・`execute_merge()`（対象ブランチの比較して交換、「ロック → prove → evaluate」の順、`merge_origin` の記録）・SHA に結びついた承認（`kind`、`direct` / `credential_origin`、`approver_native_role`、blocker `human_approval_required` / `native_maintainer_approval_required` / `native_approval_required`、CODEOWNERS のロールの指定は `native_role`）、承認・マージ・`mergeability` の REST と冪等性、`merge_blocked`、**§4.0 のすべて**（`Kind`、`Credential` の全種、`ActAs` / `Reserved` / `Queued` / `JobToken` / `Internal` の許可リスト、エージェントと宣言されたトークンとサービスアカウントのマージの deny、`role_for()` / `native_role()`、spec-lane の読める範囲のローダーの規則、`required_scopes()`、`INTERNAL`、`InstanceRoute::Confirm`）、**確認の画面・主張の発行・claim（`sub`）・検証（§4.2）**、**kanflow のオーナーの能力の同期（§4.4。`kanflow_grants`、鮮度の SQL、上限と保留、Owner への通知、観察のジョブ、doctor）**、`ProtectedPathManage`、`IntegrationBindingManage`、**§4.5 の行と代理・ボット・kanflow の出どころの生成した deny の行のすべて**（完了条件）。代理の allow は代理実行の ADR の受理後 |
| **M4** | 自動修正の状態機械とテーブル（追記のみのマイグレーション）、③④⑦（④の先の prove）、**決定的な修正**、§2 のすべて、ボットの許可リストと生成した行、pre-receive のボットの ID の拒否、「AI 変更」タブ、P-9 の `autofix`、マージキュー、MR セット、**マージの予約（§4.3、資格情報の再確認と `credential_revoked`）と `merge_reserved` / `reservation_cancelled`**、**`forge/spec-trace` を全プロジェクトで必須（§4.1.1）**、`JobLogRead` / `PipelineRead` のルート |
| **M5** | LLM による修正（パイプライン → L-2 の指摘 → 衝突の順）と品質の輪。**種類ごとに評価セットの目標を満たすまで既定は無効**（`autofix.llm.allowed_triggers`）。**`forge/spec-conformance` の影の段階**（§4.1.2、`error` の種類の記録）、GPU の予約と昇格。**AI（仕様照合）の承認（§4.1.3、DB と対象ブランチの両方で選ぶ。既定は無効）と根拠の画面、事後のレビュー、誤った承認の率の計測** |
| **M5 以降（基準を満たしたら）** | **`forge/spec-conformance` を必須に**、可用性の `error` のときだけの免除、後からの確認、縮退の切り替え |
| **保留** | **自動マージ（§4.1.6）**: 計測が §4.1.6 の開始条件を満たしたら、保護されていないブランチから別の ADR で再検討する。kanflow だけで完結する経路（案 C）も同じく必要になれば再検討 |

### 7. 計画書と他の ADR の変更（主セッションが適用する）

- `01` §3.3: 「MR の自動修正ループ | Should | M4 / M5」「kanflow からの承認・マージ（確認の画面・予約） | Should | M3 / M4」「仕様照合のチェック（spec-trace は必須、spec-conformance は影の段階の後に必須） | Must | M4 / M5」
  「AI（仕様照合）の承認 | Should | M5」「自動マージ | 保留（§4.1.6）」。§6.1 の「出力が操作を起こすことはない」を本 ADR の文に、「LLM の判断だけでマージしない」を §4.1.5 の文に。
  §6.2: L-5 を Should にし内容を §1.2 に、L-10「自動修正」を追加、L-3 に「受入基準ごと・根拠の行付き・影の段階の後に必須・可用性の `error` のときだけの免除」。§6.6 に優先度、MR レビューの層の GPU の予約、§3 のインスタンス全体の上限。
  §8: M3〜M5 に §6 の範囲、M8 から L-5 を外す、M5 の完了条件に目標値。§2 か §3.3 の冒頭に §0 の全体の流れ（ユーザーの要約の文と図）。
- `01` §3.2（組織・ユーザー・権限）: 行「kanflow のプロジェクトのオーナーに、結びつけた forge のプロジェクトでのマージ・承認・キュー・予約・読み取りの能力を同期（直接の push・管理の操作は不可、4 時間で失効、追加は Owner に通知） | Should | M3 | ADR-0017 §4.4」。
  行「エージェントと宣言されたトークン・サービスアカウントのトークンはマージ・承認できない | Must | M3 | ADR-0017 §4.0」。行「保護ブランチの `required_native_approvals` | Should | M3 | ADR-0017 §2.7」。ロールの行の備考に「判定は `native_role()` と `role_for(action)`」。
- `03` §5.4: `forge/spec-trace` を「必須（全プロジェクト）」に、触れる要件の定義と合格の条件を §4.1.1 に。`forge/spec-conformance` を「LLM（影の段階 → 必須チェック、承認ではない、安全の制御ではない）」に、受入基準ごとの判定と根拠の検証、`error` の種類と免除を §4.1.2 に。
  §5.1 ⑤ に AI（仕様照合）の承認とその範囲、⑥ に「マージは人間の操作」。§7.1 のバインディングを「`IntegrationBindingManage`（`native_role` の Maintainer、セッションだけ）。kanflow からの代理の登録は代理実行の ADR で決める」に、
  「能力の同期の有効化は forge の Owner だけ・セッションだけ・代理では不可、結びつけの先の変更で無効」を追記。§8 の M4 / M5 に §6 の範囲。§9 の決定事項に 2026-10-28 の決定 9（影の段階を含む）。
- `04` §2.1 C-4・§2.2・§8 の決定 3 → 本 ADR（ユーザー決定）。§4.3 に P-11、P-9 に `autofix`。§3 M-3 に「投入はロックを取り、head が変わったら外す」と **予約（§4.3）**。§7 を更新。
- `02` §5.2 に §4 の表（画面のルートと claim を含む）、§5.3 に `Scope::MergeApprove` / `MergeExecute`、代理の許可リスト、最低の要件、**確認の画面・不透明な主張・裏の経路の claim と `sub` の照合・intent とルートの対応（§4.2）、案 B・C を採らない理由**、
  §5.4 の Checks API を `CheckRunWrite`（`kanflow/*` の名前空間）に、§5.8 にイベント（`merge_reserved` / `reservation_cancelled`、`approval_kind`、`merge_origin`、`project.membership_synced`、`check.waived`）、F-24 に順のマージ、
  **kanflow の側の作業（`state` を作って利用者と結びつけ確認の画面を開く、claim で主張と `sub` を受け取り照合して送る、完了の通知を受け取る、`project.members_updated` を送る、`state` をログに残さない）**、§9 にコードを追加。
  **§4.2 の `GET /projects/{project_id}` に「メンバーとロール（owner を含む）、変わらない ID（LDAP の ID / `employeeNumber`）と email」の応答の形を確定し、能力の同期（§4.4）を新しい節に**。§5.6 の受信のイベントに `project.members_updated`。§8 の kanflow 側の前提に「ロール owner の定義、変わらない ID、変更の通知」。
- **ADR-0003**: §4.1.5 の文で Decision の 1・2 項目めを改訂し「ADR-0017 で改訂」の注記。**ADR-0004**: §4.1.5 の文で Risks を改訂し、Decision に 1 行、「ADR-0017 で改訂」の注記。
  **ADR-0010**: `Kind`、`Credential` の全種と許可リスト、`ProjectView.role` → `native_role()` / `role_for(action)`、`MembershipChange.from` は `native_role`、`required_scope()` → `required_scopes()`（`All` / `Any` / `SessionOnly`）、新しい Action（§4.5）。
  **ADR-0015**: `InstanceRoute::Confirm`、`app::INTERNAL` と `authz-coverage` の確認、`SessionOnly`。
  ADR-0005・0006 に「ADR-0017 で改訂 / 拡張」の注記、`Git/docs/adr/README.md` の行を accepted に。ADR-0008 に「ボットの ID・トレーラーを持つ人間の push の拒否」「保護パスは ADR-0017 §2.7 で定義」「保護ブランチの `required_native_approvals`」を追記。
  ADR-0016 に `merge_reserved` / `reservation_cancelled`、`approval_kind`、`merge_origin`、`role_source`、`project.membership_synced`、`check.waived`、§5 の監査、`via_assertion_id` = 主張の ID を追記。
- エージェントモデル（先に入れる）: `AREAS` に `INT`・`LLM`・`REVIEW`、`authz-matrix-auditor` の eval に「ボットの生成した deny の行」「`Api` が `MergeExecute` を満たさない」「`All` のスコープの片方だけで通る」
  「代理で保護ブランチを直接の資格情報なしにマージ」「**AI の承認が保護ブランチの人間の承認・CODEOWNERS・`native_maintainer_approval_required` を満たす**」「**トークン・代理で主張を発行できる**」「**予約の実行で元の資格情報を確かめない**」「**人間が `MergeRequestAiApprove` を通る**」
  「**kanflow の出どころの付与で `RepoPush` や許可リストの外の Action が通る**」「**判定の時点で kanflow を呼ぶ / 鮮度をジョブで判定する**」「**同期が forge の所属を変える**」「**git を読んだ後に authorize する**」「**`INTERNAL` の Action を `PLANNED` に置く**」「**エージェントと宣言されたトークンがマージする**」「**spec-lane が固定していない仕様のプロジェクトを読む**」、
  `security-reviewer` の eval に「**主張の使い回し・別の intent・別の本文・別の SHA**」「**主張を URL・opener に渡す / `state` をログに残す**」「**claim の `sub` を照合しない**」「**確認のボタンがクリックの時機の攻撃に弱い**」「**同期の失敗で能力を与える・消えない**」
  「**AI の承認のカテゴリが実行されるファイルを含む / 埋め込みの判定が開く側に倒れる**」「**`invalid_output` を免除する**」、`llm-integration-reviewer`（M5）に §2 と §4.1.2 の根拠の検証。

## Alternatives Considered

### 提案のブランチだけ作る（ADR-0006 の C-4 の形）
- **Pros**: ソースブランチを機械が触らない
- **Cons**: 取り込みの操作が毎回要り、MR が止まる時間が減らない
- **Why not**: 決定 1。安全は候補での検証、ロックの中の確かめ直し、SHA に結びついた承認、revert で担保する

### 外部のエージェントが診断 API だけを読んで直す（現在の計画）
- **Pros**: forge は GPU を使わない
- **Cons**: エージェントを持たない利用者には効かず、指摘と衝突が放置され、結果を測れない
- **Why not**: 両立させる。P-9 の `autofix` で競合を避ける

### LLM に道具を持たせ、サンドボックスで操作させる
- **Pros**: 複雑な失敗も直せる可能性がある
- **Cons**: 注入された文が道具の権限を得る。27B の道具の利用は不安定で GPU を長く占有する。手順が毎回違い辿りにくい
- **Why not**: 状態機械 + 構造化された出力のほうが診断できる。評価で限界が見えたら別の ADR で再検討する

### Runner が作ったコミットをそのまま使う
- **Why not**: 侵害された Runner がメッセージ・親・トレーラーを偽れる。tree だけを受け取り、サーバーがコミットを作る

### 承認の取り消しをイベントで行う（後の DB の書き込みに頼る）
- **Why not**: 反映と取り消しの間に隙間ができる。承認を SHA に結びつけ、構造で取り消す

### 先に積んでから確かめる / rebase で衝突を解く / サーバーで適用する
- **Why not**: 壊れた AI コミットが残る / 人のコミットの書き換えと force push が要る / リポジトリのコードをサーバーで実行する

### kanflow からのマージを専用の規則で作る
- **Why not**: マージ条件が 2 か所になり片方だけ緩む。同じ Action と `evaluate_merge()`、主体の種類はマトリクスの行で区別する

### ②の AI の仕様照合を承認として数える（すべての MR で）
- **Pros**: 要件に触れる MR のレビューの手間が最も減る
- **Cons**: LLM の判断だけで承認が成立し、差分に書いた文で誤らせれば人間のレビューなしにマージに近づく（脅威 d）。27B の照合の精度は未計測
- **Why not**: 決定 9。②はチェック（止める側）にだけ使い、承認として数えるのは③の機械的に範囲を決められるカテゴリに限る

### ②を導入の時点から必須にし、免除を設けない
- **Pros**: 例外の経路がない
- **Cons**: ADR-0003 が「常に必須」を退けた理由（誤検知と LLM の停止が開発の停止になる）がそのまま当てはまる。GPU の停止・偽の `failure` で、障害の修正を含む要件に触れるすべての MR が止まる
- **Why not**: ユーザー確認（2026-10-28）のとおり、影の段階で測ってから必須にし、可用性の `error` のときだけ条件付きの人間の免除を設ける

### `error` ならどの種類でも免除できる
- **Why not**: 健全なゲートウェイでの出力の不正は、モデル・プロンプト・入力の品質の問題で、差分の文で意図的に起こせる（注入で免除の経路に誘導できる）。免除は可用性の失敗だけに限る

### AI の承認をすべての変更・すべてのカテゴリで数える / 対象ブランチのファイルだけ、または MR の中で有効にする
- **Why not**: 保護パス・マイグレーション・CI の設定の変更を AI が承認できると、権限・データ・実行の境界を人が見ずに変えられる。ファイルだけで有効にできると、Developer と任意の 1 人の承認で有効にできる。
  MR の中で有効にできると、その MR 自身が自分を承認の対象にできる。DB（`native_role` の Maintainer）と対象ブランチの両方、設定で緩められない除外に限る

### `docs_only` に `**/*.md`、`test_only_addition` にテストの支援のファイルを含める / 埋め込みの判定を開く側に倒す
- **Why not**: `include_str!` の doctest、エージェントの指示、Runbook、テストの支援のファイルは実行されるか、人や AI の操作を動かす。登録していない埋め込みの仕組みは網羅できないので、検出したら `docs_only` を使えないようにする

### AI の承認で保護ブランチの人間の承認・CODEOWNERS を満たす
- **Why not**: 保護ブランチの最低限の人の目がなくなる。`ai_spec` は数の 1 件にだけ数え、`human_approval_required`・CODEOWNERS・`native_maintainer_approval_required`・`required_native_approvals` は人間だけが満たす

### AI の承認の後に自動でマージする（④を今入れる）
- **Pros**: docs・テストの追加・パッチ更新が人の操作なしに流れる
- **Cons**: 誤った承認の率が分からないまま、人の操作が 1 つもないマージが生まれる
- **Why not**: 決定 9 ④。§4.1.6 の計測の基準を満たしてから、保護されていないブランチから別の ADR で再検討する（保留）

### エージェント・サービスアカウントのトークンにもマージを許す（スコープだけで絞る）
- **Why not**: エージェントが docs の MR を作り、AI の承認が付き、エージェントのトークンがマージすると、人間が一度も関わらない（脅威 g）。マージの系統と承認は人間の主体だけに許す

### マージの出どころを `MergeState` に入れ、`evaluate_merge()` で人間かを判定する
- **Why not**: 状態の関数が主体の種類で分岐し、判定点が 2 つになる（A7）。人間でない主体の拒否は `authorize()` が行い、出どころは `execute_merge` が記録する

### 案 B: 個人のトークンを kanflow に保存し、本人の資格情報で直接呼ぶ
- **Pros**: forge の側に新しい画面も主張も要らない。「直接の資格情報」をそのまま満たす
- **Cons**: kanflow が侵害されると全員のトークンが漏れ、承認・マージ以外の権限も含めて全員になりすませる。失効と更新の運用が利用者ごとに要る
- **Why not**: 決定 10。主張は 1 つの intent・ルート・本文・SHA に 5 分・1 回だけで、漏れても被害が限られる

### 案 C: 保護されていないブランチでは kanflow の代理だけで完結させる（主張なし）
- **Pros**: 保護されていないブランチではポップアップが要らない
- **Cons**: 経路によって必要な本人の確認が変わり、1 つのサービスアカウントが保護されていないブランチで N 人分の承認を作れる
- **Why not**: 決定 10。A + D で操作は 1 回で済み、必要がない。運用で不便が分かれば再検討する（保留）

### 主張を署名した JWS にする / kanflow が作る / URL や `window.opener` で渡す
- **Why not**: forge が発行も検証もするので署名は要らず、アルゴリズム・鍵・新しい crate の危険だけが増える / kanflow が作れるなら 1 つのサービスアカウントが N 人分を作れる /
  URL は履歴・リファラー・ログに残り、`postMessage` は kanflow の XSS に読まれ、COOP と両立しない。不透明な乱数を、kanflow のサーバーが裏の経路（claim）で受け取り、`sub` を照合する

### 承認つきの予約を 1 つのルートの本文の bool で表す
- **Why not**: 1 ルート = 1 Action（ADR-0015）に反し、`reserve` の主張で `approve: true` を送って本人の承認を作れる。ルートと Action を分け、intent とルートを 1 対 1 にする

### 予約を「承認した時点の主体の証明」で後から実行する（実行時に認可し直さない）
- **Why not**: 予約から実行までにロールの喪失・退職・スコープや資格情報の取り消しが起きうる。実行の時点で本人の今の権限と元の資格情報の有効性で認可し直し、`evaluate_merge()` と SHA を確かめる

### 判定の時点で kanflow に問い合わせる（kanflow を信頼する）
- **Pros**: 同期の遅れがなく、オーナーの変更が即座に効く
- **Cons**: 判定点が kanflow の応答に依存し、純粋な関数でなくなる（ADR-0010）。kanflow の停止がマージの停止になり、kanflow の侵害で **すべての結びつけた main にマージできる**。判定の根拠が forge の監査に残らない
- **Why not**: 判定点は 1 つで forge の DB だけを読む。kanflow の情報は監査付きの能力の付与として実体化し、許可リスト・上限・保留・通知・鮮度で被害を限る

### kanflow のオーナーに Maintainer のロールを与え、管理の操作だけを拒否のリストで外す（第 1 版の案）
- **Why not**: 拒否のリストの漏れ（CI の変数・シークレット、デプロイ先、Runner、タグ、ブランチの削除、今後の Action）がすべて kanflow の側に開き、「Maintainer に許す push」で保護ブランチへ直接 push できる。
  能力の許可リストにし、他はすべて `native_role` で判定する（既定で閉じる）

### 失効を定期のジョブで `suspended` に書き換える（第 1 版の案）
- **Why not**: ジョブ・キューが止まると古い付与が効き続ける（開く側に倒れる）。鮮度をロールの SQL で判定し、ジョブは記録だけにする

### kanflow のオーナーだけがマージできる（forge の Maintainer のマージの権限を外す）
- **Why not**: ユーザーが採らなかった（決定 11、「両方がマージできる」）。kanflow の停止・誤設定で誰もマージできなくなる

### kanflow のオーナーかつ forge の Maintainer の両方を満たす人だけがマージできる
- **Why not**: ユーザーが採らなかった（決定 11）。二重の登録の手間が残り、目的（kanflow のオーナーが forge で別に登録されずにマージできる）を満たさない。
  ただし、プロジェクトが選べば保護ブランチの `required_native_approvals` で「forge の所属の人の承認を 1 件以上」を要求できる（§2.7）

### kanflow の出どころの付与を forge の所属の表に同じ行として書く（出どころを区別しない）
- **Why not**: 同期が forge で付けた所属を上書き・削除しうる。kanflow で外れた後も残る所属と区別できず、監査から「なぜこの人がマージできるか」を辿れない。別の表と `RoleSource` で分ける

## Consequences

### Positive
- エージェントを持たない利用者の MR も、fmt・lint・単純なテスト失敗・ロックファイル・単純な衝突で止まらなくなる
- AI の変更は試行・候補・監査・イベントで辿れ、ボタン 1 つで戻せる。承認の取り消しは型と SQL の構造で保証される
- PO・SM が kanflow の PBI の画面から、AI の修正の進捗を見て、確認の画面の 1 回の操作で承認とマージ（予約）まで終えられる。保護ブランチでも forge の画面に移らずに済む
- 要件に触れるすべての MR で、`@req` のテストの存在と成功が機械的に確かめられ、受入基準ごとの根拠が付き、要件の状態（`03` §5.5）の根拠になる
- 選んだカテゴリの MR はレビュアーの待ちが減る。AI の承認は種類・根拠・入力・方針の版が残り、後から誤りの率を測れる
- kanflow のオーナーは forge で別に登録されずにマージでき、kanflow で外れれば kanflow に届く間は最大 10 分、届かない間も最大 4 時間で能力が外れる。付与と削除はすべて監査とイベントに残り、追加は Owner に通知される
- 新しい Action・資格情報・主体の種類は、許可リストと生成した deny の行で既定で閉じる。すべてのマージは出どころ（主体の種類・資格情報・経路）を記録する

### Negative
- AI のコミットの後は開発者のローカルのブランチが遅れる（`git pull` が要る。push は non-fast-forward で拒否されるので失われない）
- 候補のパイプラインの分だけ Runner の負荷が増える。承認が SHA に結びつくのでレビュアーの手間が増える。シークレットを要する必須チェックは反映の後にもう一度走る
- ②が必須になった後、LLM のゲートウェイ・GPU が止まると、免除（30 分の待ち、`native_role` の Maintainer、承認 1 件の追加）までは要件に触れる MR のマージが止まる。`invalid_output` が続く MR は免除できず、プロンプト・モデルの調査が要る。
  ①が必須なので `@req` を付ける規則が全プロジェクトで必須になる
- kanflow からの操作のたびに確認の画面（forge のセッション、1 秒の待ち、保護ブランチでは番号の入力）が要る。MR セットの途中停止は手で後始末が要る。予約は期限・新しいコミット・CI の失敗・資格情報の失効で取り消され、やり直しが要る
- 実効のロールの出どころが 2 つになり、Action ごとにどちらで判定するかが変わる（`role_for` と `native_role`）。kanflow の出どころの人は、マージ・承認はできても push・管理はできない。
  kanflow の長い障害では 4 時間で kanflow のオーナーのマージが止まる（forge の Maintainer は続けられる）。設定のファイルの MR は `native_role` の Maintainer の承認が要る
- 埋め込みの判定が閉じた既定なので、`build.rs` や登録していない埋め込みのあるリポジトリでは `docs_only` を使えない

### Risks
- **27B のローカルモデルの限界**: 複数ファイルの推論、長いログ、行番号に弱く、テストを弱めて「通す」傾向がある。期待できるのは明確なコンパイルエラー、lint、
  単純な期待値の不一致、単純な衝突程度 → 種類ごとに評価セットで測り、目標まで既定は無効。M4 の決定的な修正だけで価値が出るように分ける
- 注入された文による範囲外の変更・持ち出し → 範囲は MR の差分 + 構造化された診断だけ、固定の拒否、入力の認可、秘密情報の検出、監査の `denied`
- Runner の侵害 → tree だけを受け取り、隔離領域の fsck と `diff-tree` で再検証（alert）。`deterministic_fix` の判定は Runner の出力に頼るので、MR のパイプラインと別の Runner で作る
- 共用 GPU の混雑 → インスタンス全体で同時 1 つ、GPU 時間の上限、低い優先度。②は MR レビューの層で最低の予約と昇格
- 代理実行による承認の量産 → 代理の許可リスト、forge だけが発行する 1 回限りの主張、intent・ルート・本文の結びつけ、claim の `sub` の照合、保護ブランチでの直接の資格情報、代理実行の ADR と `security-reviewer`
- **②の誤判定**: 偽の `success`（差分の文に誘導される）→ 根拠はコメントでないコードの行だけ、①の決定的なチェックも必須、③はカテゴリに限る、②は安全の制御ではないと明記。偽の `failure` で MR が止まる → 影の段階で率を測ってから必須化、再実行、「誤り」の評価を品質の輪に。`failure` と `invalid_output` の免除は作らない
- **③の誤った承認** → カテゴリは実行されないもの・人の手順を変えないものに狭め（埋め込みは閉じた既定）、エージェントの関わる MR は対象外、マージは人間の操作、停止、`policy_version`、§4.1.6 の計測
- **宣言しないエージェント**: `agent` を宣言せずに発行した個人のトークンでエージェントが動くと、人間のトークンと区別できない → トークンの発行の画面と規約で宣言を求め、③は「作成者・push した主体」の判定で宣言済みのものだけを外す。完全な防止はできない（残る危険）
- **侵害された kanflow**: 主張の量産・使い回し・本文の差し替え → forge だけが本人のセッションで発行、intent・ルート・本文・SHA に結びつけ、1 回限り。クリックの時機を狙う攻撃 → 1 秒の待ちと新しい操作、既定のフォーカスなし、保護ブランチでは番号の入力。
  別の MR を確認させる → 画面は forge のデータと受け取る連携の名前だけを表示する。別の人の主張を混ぜる → kanflow の `sub` の照合（kanflow が侵害されていればこの照合は効かないが、主張は押した本人の操作の分しか作れない）
- **侵害された kanflow によるオーナーの書き換え**（脅威 f）→ 与えるのはマージ・承認・キュー・予約・読み取りの能力だけで、push・管理・シークレットには届かない。1 回 5 人・24 時間 10 人の上限と Owner の確認、追加ごとの Owner への通知（検出であり防止ではない）、
  マージには保護ブランチの規則・他の人の承認・`required_native_approvals`（選んだ場合）・（kanflow からは）本人の forge のセッションの主張が要る、緊急停止で即座に数えなくなる
- **ID の再利用・誤った突き合わせ** → 変わらない ID を必須のキーにし、email と食い違えば `ambiguous` で飛ばす、無効なユーザーは飛ばす

## Error codes and runbooks affected

- 新規の登録（実装時）: §5 の表（M3 で REVIEW / WEB / AUTH / INT の分（確認の画面・主張・claim・能力の同期・通知を含む）、M4 で GIT / CI の分と予約・spec-trace の分、M5 で LLM の分と AI の承認・spec-conformance・免除・後からの確認の分）。
  `FG-REVIEW-next` / `FG-INT-next` / `FG-LLM-next` / `FG-AUTH-next` は採番の時に `errors.toml` で番号を付ける。
- 新しい Runbook: `ops/runbooks/autofix-candidate-invalid.md`（該当 Runner の停止、候補の ref と隔離領域の保全）、`autofix-push-denied.md`、
  `autofix-pause.md`（緊急停止と再開。本番では承認済みの手順）、`merge-execution-failed.md`（ref と MR の状態の照合、対象ブランチの比較して交換の失敗、`merge_origin` の読み方、再実行の可否）、
  **`spec-conformance-unavailable.md`**（ゲートウェイ・GPU の確認、回復後の再実行、止まっている MR の一覧、縮退の切り替え（`SpecConformanceDegradedSet`、承認済みの手順）、免除の条件と記録の確認、`SpecConformanceRetroFailed` の対応、
  **「出力の不正」（`SpecConformanceInvalidOutput`）の調査: モデル・プロンプトの版・入力の大きさ、免除できないことの説明**）、
  **`ai-approval-pause.md`**（AI の承認の停止と再開、停止の前後に AI の承認でマージされた MR の一覧と事後のレビュー。`AiApprovalDenied` の alert もここ）、
  **`merge-reservation-stuck.md`**（予約・キュー・CI・実行時の認可と資格情報の照合、取り消しの理由の読み方）、
  **`merge-assertion-issue-failed.md`**（主張の記録の失敗、`ActAsAssertionInvalid` の理由の多発（`replayed` / `body_mismatch` / `intent_mismatch` は kanflow の侵害の兆候）、claim の失敗）、
  **`kanflow-membership-sync.md`**（`KanflowMembershipSyncFailed` の原因の切り分け（接続・TLS・認証・応答の形）、`membership_sync_runs` と監査の読み方、`KanflowMembershipSuspended` からの回復、
  `KanflowMembershipBulkHeld` の確認と Owner の確認の手順、Owner への通知の失敗、緊急停止 `IntegrationMembershipSyncPause`、突き合わせできない人の扱い、心当たりのない付与の取り消し）。
- 更新する Runbook: Runner の障害に `autofix` のジョブと `deterministic_fix` の判定のジョブ、`job-dead-letter.md`（ADR-0016）に `autofix.run`、`merge_set.run`、`merge_reservation.run`、`ai_approval.evaluate`、`kanflow.membership_sync`、`spec_conformance.retro`。
- README の更新（実装時）: `forge-core`（`evaluate_merge()`、`ApprovalKind`、`classify_ai_approval()`、`spec_trace()`、`SpecConformanceJudge`、`plan_membership_sync()`）、
  `forge-auth`（`Kind`、`Credential` の全種と許可リスト、`BotKind::allows()`、`Action::act_as_allowed()` / `kanflow_grant_allowed()` / `required_scopes()` / `is_merge_family()`、`ProjectView::native_role()` / `role_for()`、主張の検証、新しい Action・Resource・Scope）、
  `forge-db`（ロールのローダーに `kanflow_grants` と鮮度の SQL、spec-lane の読める範囲、主張の保存と消費）、`forge-git`（bundle の隔離と検証、`commit-tree`、比較と交換、隠し ref、pre-receive の拒否、埋め込みの検出）、`forge-llm`（`SpecConformanceJudge` の実装）、
  `forge-ci`、`forge-web`（確認の画面、`InstanceRoute::Confirm`、`INTERNAL`、ヘッダと `state` の伏せ字）、新しい機能クレート `forge-autofix` と、マージの実行・予約・AI の承認の worker の置き場所（`forge-review`）、
  仕様照合のチェックの置き場所（`forge-checks`。`forge-review` とはイベントと `forge-core` の型だけでつながる）、能力の同期の worker（`forge-integ`。`forge-review` とは表とイベントだけでつながる）。

## インターフェースの概略（実装は含めない）

```rust
// forge-auth
pub enum Kind { Human, Bot(BotKind), Service, System(SystemWorker) }
pub enum BotKind { Autofix, SpecLane }
pub enum SystemWorker { MembershipSync, SpecTrace }
impl BotKind { pub fn allows(self, a: Action) -> bool; }          // exhaustive match over Action, no `_`
impl SystemWorker { pub fn allows(self, a: Action) -> bool; }     // exhaustive
pub enum Credential {
    Session,
    Token { scopes: Scopes, agent: bool },                                              // agent = declared at issuance
    ActAs { via: ServiceId, via_scopes: Scopes, assertion: Option<VerifiedAssertion> }, // Action::act_as_allowed(); mutations need a matching intent
    Reserved { reservation: MergeReservationId, origin: OriginCredential },             // MergeRequestMerge only
    Queued { run: QueueRunId, origin: OriginCredential },                               // MergeRequestMerge only
    JobToken { job: JobId, repo: RepoId },                                              // RepoRead on `repo` only
    Internal,                                                                           // bots and system workers
}
pub enum CredentialOrigin { Session, Token, ActAsAssertion }                             // recorded on approvals; all three are "direct"
impl Principal {
    pub fn act_as(person: UserCtx, via: ServiceId, via_scopes: Scopes, assertion: Option<VerifiedAssertion>) -> Result<Principal, ForgeError>; // rejects bots / services (FG-INT-0006); unit-tested
    pub fn autofix(bot: BotAccount, mr: MergeRequestId, on_behalf_of: UserCtx, enabler: Option<UserCtx>) -> Principal; // worker only
    pub fn spec_lane(bot: BotAccount, mr: MergeRequestId) -> Principal;                 // worker only; loader scopes views (MR project + pinned spec projects, read-only)
    pub fn system(worker: SystemWorker) -> Principal;                                   // worker only
    pub fn reserved(person: UserCtx, reservation: &MergeReservation, origin_now: OriginCredentialState) -> Result<Principal, ForgeError>;
    pub fn queued(person: UserCtx, run: &QueueRun, origin_now: OriginCredentialState) -> Result<Principal, ForgeError>;
}
pub enum Scope { /* existing */ MergeApprove, MergeExecute }      // Api does not satisfy these
pub enum ScopeRequirement { None, All(&'static [Scope]), Any(&'static [Scope]), SessionOnly }
impl Action {
    pub fn required_scopes(self) -> ScopeRequirement; // exhaustive; replaces required_scope()
    pub fn requires_session(self) -> bool;            // derived from SessionOnly
    pub fn act_as_allowed(self) -> bool;              // exhaustive
    pub fn kanflow_grant_allowed(self) -> bool;       // exhaustive; RepoPush / IntegrationBindingManage = false
    pub fn is_merge_family(self) -> bool;             // denied to declared-agent tokens and service accounts
}

pub enum RoleSource { Forge, Kanflow }
pub struct ProjectView { /* existing minus `role` */ native: Option<Role>, kanflow_grant: bool /* fresh, computed in SQL */, read_only_scope: bool /* spec-lane */, pub via_bound: bool }
impl ProjectView {
    pub fn native_role(&self) -> Option<Role>;
    pub fn role_for(&self, action: Action) -> (Option<Role>, RoleSource); // kanflow counts as Maintainer only if action.kanflow_grant_allowed(); tie -> Forge
}

pub enum MergeIntent { Approve, Unapprove, Merge, Enqueue, Reserve, ApproveAndReserve, CancelReservation, MergeSet { steps_hash: Digest } }
pub struct RequestBinding { pub integration: IntegrationId, pub person: UserId, pub intent: MergeIntent, pub route: RouteInstance,
    pub body_digest: Digest, pub head_sha: Option<CommitSha> }
pub struct AssertionHandle(/* private: 256-bit random; only its hash is stored */);
pub struct ClaimedAssertion { pub handle: AssertionHandle, pub sub: UserId, pub sub_stable_id: StableSubjectId } // kanflow compares sub with the user bound to state
pub struct VerifiedAssertion { /* private: assertion id + binding; built only by verify_merge_assertion */ }
pub enum IntentProof { Approve(Authorized<MergeRequestCtx>), Merge(Authorized<MergeRequestCtx>), /* … one per intent */ MergeSet(Authorized<MergeSetCtx>) }
pub fn issue_merge_assertion(issue: Authorized<MergeAssertionCtx>, intent: IntentProof, state: &KanflowState, now: Timestamp)
    -> Result<AssertionHandle, ForgeError>;   // checks issue.intent() == intent.action(); SessionOnly
pub fn claim_merge_assertion(proof: Authorized<IntegrationCtx>, state: &KanflowState, now: Timestamp) -> Result<ClaimedAssertion, ForgeError>;
pub fn verify_merge_assertion(raw: &AssertionHandle, binding: &RequestBinding, store: &dyn AssertionStore, now: Timestamp) -> Result<VerifiedAssertion, ForgeError>;
    // consumption: success in the operation's ChangeTx; post-verification failure in a separate committed tx (outcome = rejected)

// Action (new): MergeRequestRead, MergeRequestApprove, MergeRequestUnapprove, MergeRequestMerge, MergeQueueEnqueue,
//   MergeReservationCreate, MergeRequestApproveAndReserve, MergeReservationCancel, MergeSetRunCreate, MergeSetRunRead,
//   MergeConfirmView, MergeAssertionIssue, MergeAssertionClaim,
//   MergeRequestAiApprove, AiApprovalEvidenceRead, AiApprovalFeedback, AiApprovalSettingsManage, AiApprovalPause, AiApprovalResume,
//   SpecConformanceWaive, SpecConformanceDegradedSet, ProtectedPathManage, InternalCheckWrite, CheckRunWrite (Checks API, PLANNED),
//   JobLogRead, PipelineRead, RequirementRead (PLANNED until their routes), IntegrationBindingManage (PLANNED until M3 route),
//   AutofixPush, AutofixCandidateWrite, AutofixComment, AutofixAttemptRead, AutofixAttemptInputRead, AutofixSettingsManage,
//   MergeRequestAutofixToggle, AutofixCommitRevert, AutofixPause, AutofixResume,
//   IntegrationMembershipSyncToggle, IntegrationMembershipSyncAcknowledge, IntegrationMembershipSyncRead,
//   IntegrationMembershipSyncPause, IntegrationMembershipSyncResume, MembershipSyncApply, MembershipSyncSuspend
// Existing (action.rs) decided on native_role: RepoPush, ProjectUpdate, ProtectedRefManage, MemberManage, ProjectVisibilityChange, ProjectTransfer, ProjectDelete, …
// forge-web: pub const INTERNAL: &[Action] = &[AutofixPush, AutofixCandidateWrite, AutofixComment, MergeRequestAiApprove, InternalCheckWrite, MembershipSyncApply, MembershipSyncSuspend];
// Resource (new): AutofixTarget { .. }, MergeRequest { project, author, ai_commit_present, head_sha_matches, committers, enabler, reserved_by, protection },
//   MergeSet { steps: Vec<MergeRequest> }, MergeSetRun(Vec<ProjectView>), AutofixAttempt { project, sources: Vec<(Action, ProjectView)> },
//   MergeAssertion { intent: MergeIntent, target: MergeTarget, integration: IntegrationId, via_bound: bool },
//   AiApprovalTarget { project, eligibility: AiApprovalEligibility, policy_version: PolicyVersion, paused: bool, head_sha_matches },
//   AiApproval { project, sources: Vec<(Action, ProjectView)> }, CheckRun { project, name: CheckName },
//   Binding { project: ProjectView, integration: IntegrationId, via_bound: bool, sync_enabled: bool }, KanflowGrantSet { binding: BindingId, sync_enabled: bool, via_bound: bool }

// forge-core
pub struct PatchProposal { pub edits: Vec<TextEdit>, pub rationale: String, pub addresses: Vec<TriggerKey> }
pub struct TextEdit { pub path: RepoPath, pub old_text: String, pub new_text: String }
pub enum AutofixOutcome { Landed(CommitSha), Discarded(DiscardReason), Proposed, GaveUp(GiveUpReason), Superseded }
pub trait PatchGenerator { async fn propose(&self, input: &AutofixInput, prio: LlmPriority) -> Result<PatchProposal, ForgeError>; } // forge-llm
pub trait CandidateVerifier { async fn run_candidate(&self, mr: MergeRequestId, candidate: CommitSha) -> Result<PipelineVerdict, ForgeError>; } // forge-ci

pub enum ApprovalKind { Human { origin: CredentialOrigin }, AiSpec { category: AiApprovalCategory, record: AiApprovalRecordId, policy_version: PolicyVersion } }
pub struct ApprovalRow { pub kind: ApprovalKind, pub user: Option<UserId>, pub sha: CommitSha, pub carried_from: Option<CommitSha>,
    pub approver_native_role: Option<Role> /* loaded at evaluation time */, pub approver_role_source: Option<RoleSource> }
pub enum AiApprovalCategory { DocsOnly, TestOnlyAddition, DependencyPatch, DeterministicFix }
pub enum IneligibleReason { CategoryNotEnabled, MixedCategories, ProtectedPath, Migration, CiConfig, FixedDeny, ModeChange, SpecPath,
    IncludeTarget, UnregisteredEmbed, TestSupport, ReqTagChanged, SecretsInPostMergeTests, SupplyChain, AiCommitPresent, AgentInvolved,
    SpecConformanceWaived, ChecksNotGreen, Paused }
pub enum AiApprovalEligibility { Eligible(AiApprovalCategory), NotEligible(Vec<IneligibleReason>) }
pub enum EmbedScan { Resolved(Vec<RepoPath>), Unresolvable(Vec<EmbedFinding>) /* non-literal or unregistered -> fail closed */ }
pub struct AiApprovalInput { pub changes: Vec<ChangedPath>, pub numstat: Vec<NumStat>, pub lockfile_diff: Option<LockfileDiff>,
    pub fixer_tree_matches: Option<bool>, pub policy: AiApprovalPolicy /* DB ∩ target-branch file */, pub path_policy: PathPolicy,
    pub embeds: EmbedScan, pub post_merge_tests_secretless: SecretlessCheck /* file + DB */, pub ai_commit_present: bool, pub agent_involved: bool,
    pub spec_conformance_waived: bool, pub spec_checks_green_at_head: bool }
pub fn classify_ai_approval(input: &AiApprovalInput) -> AiApprovalEligibility;           // pure, no LLM; called only after the read proofs

pub struct ReqTestIndex { /* @req -> test ids, from the merge-result tree */ }
pub enum SpecTraceVerdict { Success { requirements: usize }, Failure { untested: Vec<RequirementId>, failing: Vec<(RequirementId, TestId)> } }
pub fn spec_trace(touched: &[RequirementId], index: &ReqTestIndex, results: &TestResults) -> SpecTraceVerdict; // pure
pub struct ConformanceReport { pub criteria: Vec<CriterionJudgement> }
pub struct CriterionJudgement { pub criterion: CriterionId, pub verdict: CriterionVerdict, pub evidence: Vec<EvidenceSpan>, pub reason: String }
pub enum CriterionVerdict { Satisfied, Partial, NoEvidence }
pub enum SpecConformancePhase { Shadow, Required }
pub enum SpecConformanceError { Unavailable(UnavailableKind /* Timeout | BreakerOpen | Degraded */), InvalidOutput } // only Unavailable is waivable
pub trait SpecConformanceJudge { async fn judge(&self, input: &SpecConformanceInput, prio: LlmPriority) -> Result<ConformanceReport, ForgeError>; } // forge-llm
pub fn validate_evidence(report: &ConformanceReport, diff: &DiffIndex, lexers: &CommentLexers) -> ValidatedConformance; // pure: missing / comment-only lines -> NoEvidence
pub struct SpecConformanceWaiver { pub mr: MergeRequestId, pub head_sha: CommitSha, pub waived_by: UserId, pub reason: String, pub trigger: UnavailableKind }

pub struct MergeState { /* existing fields */ pub approvals: Vec<ApprovalRow>, pub branch_protected: bool, pub required_native_approvals: NativeApprovalRule,
    pub touches_repo_settings: bool, pub codeowners: Vec<CodeownerRequirement> /* role entries resolved on native_role */,
    pub current_ai_policy_version: PolicyVersion, pub spec_conformance_waived: bool /* .. */ }   // no principal, no merge origin
pub enum MergeBlocker { /* existing */ HumanApprovalRequired, DirectCredentialRequired, NativeMaintainerApprovalRequired, NativeApprovalRequired,
    ExtraHumanApprovalRequired, RequiredCheckMissing, QueueRequired /* .. */ }
pub fn evaluate_merge(state: &MergeState) -> MergeVerdict;         // pure; counts at most one AiSpec approval at head with the current policy_version
pub enum MergeVerdict { Mergeable(MergeGateOk), QueueRequired, Blocked(Vec<MergeBlocker>) }
pub struct MergeGateOk { /* private: mr, head_sha, target_sha; built only by evaluate_merge on a LockedMr */ }
pub struct MergeOrigin { pub principal_kind: KindTag, pub credential_kind: CredentialKind, pub agent_declared: bool, pub via: Option<ServiceId>,
    pub reservation_id: Option<MergeReservationId>, pub queue_run_id: Option<QueueRunId>, pub role_source: RoleSource, pub credential_origin: CredentialOrigin }

pub enum ReservationCancelReason { HeadChanged, UserCancelled, CredentialRevoked, RoleLost, UserInactive, ScopeLost, Blocked(Vec<MergeBlocker>), PipelineFailed, MrClosed, Expired }

pub struct KanflowMember { pub subject_id: StableSubjectId /* LDAP id / employeeNumber, required */, pub email: Option<Email>, pub role: KanflowRole }
pub enum KanflowRole { Owner, Other }
pub struct ResolvedOwner { pub user: UserId, pub kanflow_project: KanflowProjectId }
pub enum UnmatchedReason { Unknown, Disabled, NotHuman, Ambiguous }
pub struct KanflowGrant { pub project: ProjectId, pub user: UserId, pub binding: BindingId, pub granted_at: Timestamp }
pub struct SyncLimits { pub max_additions_per_run: u32 /* 5 */, pub max_additions_per_day: u32 /* 10 */, pub suspend_after: Duration /* 4h */ }
pub enum MembershipSyncChange { Add { project: ProjectId, user: UserId }, Remove { project: ProjectId, user: UserId } }
pub enum SyncPlan { Apply { removals: Vec<MembershipSyncChange>, additions: Vec<MembershipSyncChange> },
    HoldAdditions { removals: Vec<MembershipSyncChange>, requested_additions: u32 } /* still a successful run */, NoChange }
pub fn plan_membership_sync(desired: &[ResolvedOwner], current: &[KanflowGrant], recent_additions: &[(ProjectId, u32)], limits: &SyncLimits) -> Vec<(ProjectId, SyncPlan)>; // pure
pub trait WorkItemProvider { /* existing */ async fn project_members(&self, project: &KanflowProjectId) -> Result<Vec<KanflowMember>, ForgeError>; } // TLS verified, bounded timeout

// forge-db (role loader; freshness is part of the query, not of a job)
pub async fn load_project_view(db: &Db, principal: &PrincipalRef, project: ProjectId, now: Timestamp) -> Result<Option<ProjectView>, ForgeError>;
    // kanflow_grant = EXISTS(grant ∧ binding enabled ∧ same kanflow_project ∧ integration enabled ∧ service account active ∧ not paused ∧ last_success_at > now - suspend_after)
    // spec-lane: Some(read-only view) only for the MR project and the spec projects pinned in the target branch's .forge/repo.yml specs:

// forge-git
pub fn import_candidate_tree(proof: &Authorized<AutofixTargetCtx>, repo: &RepoHandle, bundle: &Path, prerequisites: &[CommitSha], limits: &BundleLimits) -> Result<TreeOid, ForgeError>; // proof first
pub fn commit_tree(repo: &RepoHandle, tree: &TreeOid, parents: &[CommitSha], meta: &BotCommitMeta) -> Result<CommitSha, ForgeError>;
pub fn changed_paths(repo: &RepoHandle, from: &CommitSha, to: &CommitSha) -> Result<Vec<ChangedPath>, ForgeError>; // diff-tree -r --no-renames --raw
pub fn numstat(repo: &RepoHandle, from: &CommitSha, to: &CommitSha) -> Result<Vec<NumStat>, ForgeError>;          // diff-tree --numstat
pub fn scan_embeds(repo: &RepoHandle, at: &CommitSha) -> Result<EmbedScan, ForgeError>;                         // literal include_str!/include_bytes!, registered doc-build configs; anything else -> Unresolvable
pub fn check_path_policy(changes: &[ChangedPath], policy: &PathPolicy) -> Result<(), PolicyViolation>;            // shared by autofix, AI approval and the repo-settings blocker
pub fn check_conflict_only_diff(repo: &RepoHandle, head: &CommitSha, target: &CommitSha, candidate: &TreeOid) -> Result<Vec<ChangedPath>, ForgeError>;
pub fn update_ref_cas(repo: &RepoHandle, r: &RefName, new: &CommitSha, expected_old: &CommitSha) -> Result<(), ForgeError>; // also used for the target ref in execute_merge

// forge-review
pub async fn execute_merge(proof: Authorized<MergeRequestCtx>, gate: MergeGateOk, locked: LockedMr) -> Result<MergeOutcome, ForgeError>; // the only executor; target CAS; records MergeOrigin from the proof
pub async fn reserve_merge(proof: Authorized<MergeRequestCtx>, req: ReserveRequest, locked: LockedMr, tx: &mut ChangeTx) -> Result<MergeReservation, ForgeError>;
pub async fn approve_and_reserve(proof: Authorized<MergeRequestCtx> /* MergeRequestApproveAndReserve */, req: ReserveRequest, locked: LockedMr, tx: &mut ChangeTx) -> Result<MergeReservation, ForgeError>;
pub async fn cancel_reservation(locked: &LockedMr, reason: ReservationCancelReason, tx: &mut ChangeTx) -> Result<(), ForgeError>;
pub async fn cancel_reservations_for_credential(credential: CredentialId, tx: &mut ChangeTx) -> Result<usize, ForgeError>; // in the revocation ChangeTx
pub async fn run_reserved_merge(reservation: MergeReservationId) -> Result<MergeOutcome, ForgeError>;
    // worker: lock -> Principal::reserved (current role, active, origin credential valid) -> prove(MergeRequestMerge) -> evaluate_merge -> head == reservation.sha -> execute_merge
pub fn can_merge(principal: &Principal, ctx: &MergeRequestCtx) -> bool;                // only via authorize(MergeRequestMerge); used by mergeability
pub async fn issue_ai_approval(proof: Authorized<AiApprovalTargetCtx>, record: AiApprovalRecord, locked: LockedMr, tx: &mut ChangeTx) -> Result<ApprovalId, ForgeError>;
pub async fn waive_spec_conformance(proof: Authorized<MergeRequestCtx> /* SpecConformanceWaive */, waiver: SpecConformanceWaiver, locked: LockedMr, tx: &mut ChangeTx) -> Result<(), ForgeError>;

// forge-integ
pub async fn run_membership_sync(kanflow_project: KanflowProjectId, trigger: SyncTrigger) -> Result<SyncOutcome, ForgeError>;
    // fetch (bounded, TLS) -> resolve by stable id -> plan_membership_sync -> prove(MembershipSyncApply) -> one ChangeTx per forge project
    //   (grants + audit + project.membership_synced + in-app notification to native Owners; last_success_at advances also when additions are held)
pub async fn observe_stale_syncs(now: Timestamp) -> Result<usize, ForgeError>;          // records suspended/resumed + alert; never decides access
pub enum SyncTrigger { Schedule, KanflowNotification }
```

## レビューで変えた点（security-reviewer + authz-matrix-auditor、1 回目 BLOCK）

| 指摘 | 反映した箇所 |
|---|---|
| C1 範囲がログで広がる、実行される設定の拒否漏れ | §2.1: 範囲は MR の差分 + 対象ブランチのジョブの構造化された診断だけ、固定の拒否を拡張、`.forge-ci.yml` が実行するファイル、規則は DB と対象ブランチからだけ |
| C2 入力に権限がない（IDOR） | §2.3: すべての入力を `prove(Read*)`、P-7 の範囲、`AutofixAttemptInputRead` を分けた |
| S1 Runner の出力を信じている | §1 ④: tree だけを使い `commit-tree`、隔離・fsck・上限・前提、`diff-tree --raw`、モード、決定的な修正も同じ経路、マージの親の矛盾を解消 |
| S2 悪意のあるマージ | §1.2: サーバーが `merge-tree` を計算し、衝突の hunk の外の違いを拒否、`--remerge-diff` |
| S3 反映の TOCTOU、承認の取り消しが後の書き込み頼み | §1 ⑦ のロックと確かめ直し、§2.6 の SHA に結びついた承認、キューの投入のロック |
| S4 テストの弱体化と進歩の判定の迂回 | §2.2: JUnit の件数、失敗した ID の成功、失敗したテストのファイルの編集を拒否 |
| S5 秘密情報 | §2.4 |
| S6 ボットの資格情報と ID | §2.5: 資格情報なし、worker だけが作る主体、読み取りだけのジョブのトークン、予約した ID と pre-receive の拒否 |
| S7 Action と行の不足 | §4 の表に `AutofixCandidateWrite`、`AutofixComment`、`AutofixPause`、`MergeSetRunRead`、`mergeability` |
| S8 代理と冪等性 | §4: キーの構成、認可が先、主張、保護ブランチでの直接の資格情報 |
| A1 `Credential::ActAs` | §4.0 と概略。対象が人間であることをコンストラクタで確かめる |
| A2 ボットの種類と許可リスト | §2.5: `Kind`、許可リスト、`Action::ALL` からの生成。C2 のため読み取りの Action も許可リストに入れた |
| A3 `Resource::AutofixTarget` | §2.5（enabler のロールも追加）。候補の ref も `AutofixCandidateWrite` で認可 |
| A4 Action の一覧と REST の行 | §4 の表（Action / ResourceCtx / スコープ）。blockers が見せる範囲を明記。MR セットは全件を判定 |
| A5 スコープ | §4.0: `MergeApprove` / `MergeExecute`、`Api` は満たさない |
| A6 自己承認 | §2.6: 作成者は常に deny。「共同作成者」は enabler とコミッターに広げ、設定「コミッターの承認を数えない」（既定で有効）で扱う |
| A7 `authorize()` と `evaluate_merge()` | §4.0 と概略: 状態の関数は主体を取らない、実行は 1 つで両方の証明を取る |
| A8 revert | §2.6: `RepoPush` と同じ判定、保護 ref は deny、承認は SHA で取り消される |
| A9 M3 の完了条件 | §4 と §6: 代理の deny の行をすべて M3 で書いてテスト |
| MEDIUM / LOW / 整合 | 1 回目の表のとおり（§3 の上限、§2.6 の有効化、LLM の文、隠し ref、内部のミラー、永久の記録、パスの正規化、冪等のキー、`Signed-off-by`、削除の拒否、ADR-0015 / 0016 / 0018 との整合） |

## 第 2 回のレビューで変えた点（security-reviewer + authz-matrix-auditor、2 回目 BLOCK）

2026-10-28 のユーザー決定 9〜11 と全体の流れの反映を受けたレビュー。S = security、A = authz。

| 指摘 | 反映した箇所 |
|---|---|
| S-C1 / A-HIGH kanflow のオーナーにロールを与え、拒否のリストで外していた（push と今後の Action に開く） | §4.4: **能力の許可リスト**（`Action::kanflow_grant_allowed()`、全列挙）。他はすべて `native_role()`、`RepoPush` を与えない。§4.0 の `role_for()` / `native_role()`（`ProjectView.role` を廃止）。§4.5 の生成した行 |
| S-H1 追加の上限が 1 回分だけ | §4.4 ⑤: 1 回 5 人 + 24 時間 10 人、保留は `native_role` の Owner の確認。有効化の時に最初の一覧を確認 |
| S-H2 / A-HIGH 失効がジョブ頼み・無効化と解除の扱いがない | §4.4: 鮮度をロールの SQL で判定。観察のジョブは記録だけ。無効化・解除は同じ ChangeTx で付与を消す |
| S-H3 intent と本文が主張に結びついていない | §4.2: intent ↔ ルートを 1 対 1、`body_digest`、代理では `commit_message` を受け付けない |
| S-H4 ポップアップがクリックの時機の攻撃に弱い | §4.2 の画面の守り |
| S-H5 予約の実行で元の資格情報を確かめない | §4.3: 元の資格情報の有効性、`credential_revoked`、`expires_at` = min（24 時間、資格情報の期限） |
| S-H6 リポジトリの中の設定が `native_role` を迂回する | §2.7 の blocker、§4.1.3 の DB ∩ ファイル |
| S-H7 AI の承認のカテゴリが実行されるものを含む | §4.1.3 の表 |
| S-H8 「マージは人間」が認可で強制されていない | §4.0 の資格情報の表、§4.1.3 の `agent_involved` |
| S-H9 ②が導入から必須で免除がない | §4.1.2 の段階と免除、§3 の GPU の予約 |
| A-CRITICAL git の起動の後に authorize | §1 ④ の先の prove、③ の `JobToken`、§4.1.3 の順序 |
| A-HIGH 代理の許可リスト / 内部の操作 / ローダーのテスト / 1 ルート = 1 Action / `INTERNAL` / 確認のルートの登録 / `issue_merge_assertion` / `Reserved` / 代理の取り消し / Action の名前 | §4.0、§4 の表、§4.2、§4.3、§4.5 |
| S-M / A-M / S-L / A-L | 第 2 回の版のとおり（不透明な主張、claim、`via_bound`、`rejected` の消費、変わらない ID、結びつけの先の変更、根拠のコメント、モード、サプライチェーン、保護パス、フィードバック、`policy_version`、対象の比較して交換、チェックの名前、候補の必須チェック、`MembershipChange.from`、同値、ボットの view、全列挙、停止の分割、IDOR、ロックの順序、`can_merge`、§4.5、伏せ字、戻り先、部分的な成功、通知の流量、文言、コンストラクタのテスト） |

## 第 3 回のレビューで変えた点（security-reviewer + authz-matrix-auditor、3 回目 CRITICAL / HIGH なし）

| 指摘（MEDIUM） | 反映した箇所 |
|---|---|
| ユーザー確認（2026-10-28）: ②の影の段階 → 基準で必須、免除は設計どおり | Context の決定 9、§4.1.2、§4.1.5、Alternatives。「確認が要る」の印を外した |
| A `JobLogRead` / `PipelineRead` / `RequirementRead` が未定義 | §2.3: 本 ADR で Action と最小のロールを定義、ルートは M3 / M4 の ADR、それまで `PLANNED` と deny の行（§4.5） |
| A 内部の呼び出しの `CheckRunWrite` が `INTERNAL` にない | §4.0 / §4.1.1 / §4.1.3: `InternalCheckWrite` を `INTERNAL` に分け、Checks API の `CheckRunWrite` は `PLANNED`（`kanflow/*` の名前空間） |
| A spec-lane の主体の読める範囲が未定義 | §4.0 のローダーの規則: MR のプロジェクト + 対象ブランチの `.forge/repo.yml` の `specs:` で固定した仕様のプロジェクト、読み取りだけ。§4.5 と概略の `load_project_view` |
| A `IntegrationBindingManage` が未定義 | §4 の表と §4.5: `native_role` の Maintainer（同期が有効なら Owner）、`SessionOnly`、代理と kanflow の出どころは deny、ルートまで `PLANNED`。`03` §7.1 の代理の登録は代理実行の ADR へ（§7） |
| A kanflow の付与の追加を Owner に通知、残る危険 | §4.4 ⑦、`KanflowGrantNotificationFailed`、Risks |
| A スコープを all / any / session-only で表せない | §4.0: `required_scopes() -> ScopeRequirement`、§4 の表の列、§4.5 の生成した行、概略 |
| S 免除は可用性の失敗だけ | §4.1.2: `unavailable` と `invalid_output` を分け、`invalid_output` は免除しない（`SpecConformanceInvalidOutput`、Runbook）。Alternatives |
| S マージ後のテストのジョブのシークレットをファイルと DB で確かめる | §4.1.3 の `test_only_addition` |
| S `docs_only` の埋め込みの除外が開く側に倒れる | §4.1.3 の `docs_only`: 定数でない埋め込み・登録していない仕組みを検出したら使えない（`EmbedScan::Unresolvable`、`unregistered_embed`）。Negative |
| S ADR-0003 の文を「エージェントと宣言されたトークン」に、マージの出どころを記録 | §4.1.5 の文。`MergeOrigin` を `execute_merge` が証明から作り `merges`・監査・イベントに記録（**`MergeState` には入れない**: `evaluate_merge()` が主体で分岐しないことを保つ。Alternatives に理由）。宣言しないエージェントは Risks |
| S CODEOWNERS のロールの指定と、保護ブランチの `native_role` の承認 | §2.7: CODEOWNERS のロールは `native_role` で解決、`required_native_approvals` と blocker `native_approval_required` |
| S 追加を保留した実行は成功として数える | §4.4 ⑤、`SyncPlan::HoldAdditions` の注記、`KanflowMembershipBulkHeld` |
| S claim が `sub` を返し kanflow が照合する、`state` をログで伏せる | §4.2 の流れ・ログ・kanflow の側の作業、`ClaimedAssertion`、§7 の `02` |
| LOW（安く直せるもの） | §4.5 に `All` の片方だけのトークンの行、`MergeExecutionFailed` に `merge_origin`、イベント・監査から `state` を除外、doctor の `error` の種類ごとの率、Runbook に心当たりのない付与の取り消し |

**意識して後回しにしたもの（理由）**:
- 影の段階の長さ（8 週間）と必須化の基準の値は **提案** で、M5 の計測で確定する（段階を踏むことはユーザーが確認済み）。
- `02` / `03` で計画中の他の代理の操作（ブランチの作成、結びつけの登録）を `act_as_allowed()` に入れるかは、代理実行の ADR で決める（本 ADR の範囲は承認・マージの系統。`IntegrationBindingManage` は今は代理を deny）。
- `authz-coverage` のゲートが `INTERNAL` の「worker から prove される」ことを確かめる仕組み（記録する偽の authorizer か静的な検索か）の詳細は、M3 の実装とエージェントモデルの変更で決める。
- コメントの判定と埋め込みの検出の言語・仕組みの範囲は、登録したものだけで始める（登録していないものは根拠にしない・`docs_only` を使えない、という閉じた既定で安全側）。
- 宣言しないエージェントのトークンの検出は、トークンの発行の規約に頼る（技術的な完全な区別はできない。Risks）。
