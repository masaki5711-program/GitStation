# 11 M3 の計画 — MR とレビュー、kanflow 連携の第 1 段、R1（パイロット開始）

作成: 2026-11-02（planner）。状態: **承認済み（2026-11-02）** — 利用者が §3 の推奨と §10 の推奨をすべて採用し、「テストまで一気通貫で進める」と指示した。§10 のうち事実の情報が要る問い（Q1・Q6・Q7・Q11・Q12・Q14）は、その MR に着手する前に聞く。
参照: 01 §3・§4.3・§5・§8・§9・§11、02 §5・§7・§8・§11〜§13、03 §4〜§8.1・§10、04 C-1・C-2・M-6・§8、06 §4・§7.1、08、09 §8、ADR-0005・0008・0010・0015〜0023、引き継ぎ `docs/handoff/2026-11-02-m3-start.md`。

## 0. 前提の点検

### 0.1 目標（1 文）

forge 自身とパイロットのチームの開発を GitStation の MR（レビュー・承認・条件付きのマージ）で回し、kanflow の「開発」タブが GitLab ではなく GitStation から取りこぼしなく読み、壊れたときに DB とリポジトリを整合したまま戻せることを訓練で確かめる。

### 0.2 決定済み（組み込む。議論し直さない）

| 日付 | 決定 |
|---|---|
| 2026-11-02 | GitStation `claude/brave-cray-w62c35` を `main` に統合済み（`badde83`）。M3 のブランチは `main` から切る |
| 2026-11-02 | **DB はすべて PostgreSQL 16 + TimescaleDB 2.28.1**（`timescale/timescaledb:2.28.1-pg16`）。MySQL は使わない。GitStation の dev と CI は切り替え済み（`9be4422`、`ops/dev/compose.yml` で確認） |
| 2026-11-02 | **Kanflow v2 は今 PostgreSQL へ移す**（Kanflow 側の最初の項目）。本番の Kanflow（このサーバーの :3000、MySQL）は、承認された別の切り替えまで MySQL のまま。移行の道具・リハーサル・runbook は M3 で作るが、**切り替えそのものは M3 の実装の範囲外**（人の承認が要る本番の変更） |
| 同 | Kanflow v2 は同じ Kanflow のリポジトリで、試験用のインスタンスとして別に動かす（worktree、専用のポート、専用の PG、合成データ、PostgreSQL の Keycloak の試験 realm）。GitStation の試験用インスタンスもテスト用の DB のクラスタと分ける |
| 同 | **順序: GitStation 側が先**（MR → Webhook とイベントの履歴 API → 代理実行）、**Kanflow 側はその後**（PG 移行 → GitStationSource と provider `gitstation` → F-36 の承認ボタン） |
| 同 | 代理実行（02 §5.3）は ADR と `security-reviewer` が必須。MR の承認が仕様の承認の経路（ADR-0023）。kanflow からも承認できる（F-36） |
| 同 | **§3 と §10 の推奨をすべて採用**。GitStation と Kanflow v2 は**このサーバーで作り込み、完成したら本番サーバーにビルドする**。その時に**今の Kanflow のデータを引き継ぐ**（MySQL → PG。§10 Q2 は案 a） |
| 同 | Kanflow の K1（PG 移行）は GitStation の MR と**並行して始める**（利用者の指示「並行して Kanflow でも進められることは進める」）。作業場所は worktree `D:\APP\Kanflow-v2`（ブランチ `feat/L4-db-postgresql`）、本番の `D:\APP\Kanflow` には触れない |

### 0.3 計画書どうしの食い違いと、この計画での扱い

1. **02 §5.3 の `Forge-Act-As: user:<email>`** は ADR-0021（メールだけで利用者を結ばない）と矛盾する。この計画では相手を **GitStation の利用者 ID（`usr_…`）か社員番号**で指す（kanflow の `User.employeeNumber`、02 §8.4 K-7）。正式には代理実行の ADR（MR-S1）で決める。
2. **02 §8.1 の「GitLab 互換の API」は要らなくなった。** kanflow は既に `DevSource` の抽象を持つ（kanflow v2 の `app/src/services/git/dev-source.ts`）。GitStationSource がイベントの履歴 API を直接読めばよく、GitLab の API の再現（7 種のエンドポイント）は作らない。作る量と攻撃面が減る。§10 Q13 で確認する。
3. **`forge/spec-drift` の時期**: 03 §8 の表は M4、03 §8.1 と 01 §8 は M3 の (b)。**01 §8 を正として M3**（R1 には不要）。
4. **ADR-0017 決定 11（kanflow のプロジェクトのオーナーへの forge のマージの能力）と決定 10 D（マージの予約）**はマージキュー（M4）が前提。M3 では作らず M4 に送る。
5. **IoT の仕様の DB の決まり**は `10-pfs/01-pfsdb-structure-and-conventions.md` §2「Datastore topology」（PostgreSQL + TimescaleDB、自前、MySQL は使わない）と `00-foundation/03-system-architecture.md` の「Fixed」の行。
6. **試験サーバーの試験の結果**: `docs/deploy/test-server-guide.md` §6 はチェックリストで、結果は書かれていない。見つかった問題として読めるのは §9 の 8（OIDC 以外で 2 人目の利用者を作る経路がない）と 9（`forge admin` の DB ロールの runbook とコードの食い違い）だけ。**ほかは未確認**で、利用者から受け取る。

### 0.4 コードで確かめたこと（`main`）

- クレート: `forge-auth`・`forge-cli`・`forge-client`・`forge-core`・`forge-db`・`forge-errors`・`forge-git`・`forge-hosting`・`forge-oidc`・`forge-web`。01 §4.3 の `forge-review`・`forge-checks`・`forge-integ`・`forge-notify` はまだ無い。
- `forge.users.kind` は既に `human`・`service_account`・`bot` を受ける（`20261028100100_users_and_namespaces.sql`）。サービスアカウントの表は新しく要らない。
- `forge.protected_refs` は `pattern` と `push_min_role` だけ（`20261031100000_…sql`）。承認数・必須チェックは別の表で足す（追記のマイグレーション）。
- 監査の行に代理実行の列が予約済み（`actor_via_id`、`act_as_claimed`、`act_as_key_id`、`via_assertion_id`。ADR-0016）。
- `forge-git::version::MINIMUM` は 2.38。`merge-tree --write-tree`（ADR-0006）が使える。
- `forge-git::command` は「revision の後に `--end-of-options`、パスの後に `--`」。**revision だけでパスが無いときは `--` を付けない**（`command.rs:281-288`）。§0.6 の Windows の失敗の原因の候補。
- `forge-web::app` の `PLANNED` は `UserCreate`・`AuditRead`・`Metrics`。別の Action の中で判定する Action は `DECIDED_IN_ROUTE`（`SessionCreate`、`SessionContinue`）。
- `errors.toml` に `INT`・`REVIEW` の領域のコードはまだ無い。02 §11.3 が FG-INT-0001〜0012 の番号を予定している。
- kanflow v2: `schema.prisma` の provider は `mysql`、`@@fulltext` 8 か所、`@db.MediumText` 4 か所。生の SQL 10 か所（`ON DUPLICATE KEY UPDATE`、バッククォート、引用符なしの PascalCase の表名）。`api-v1.md` §feedback に「MySQL の照合順序は大文字小文字を区別しない、PG では `contains` に `mode: "insensitive"` が要る」。`git_integration.provider` は `gitlab` だけを受ける。GitStationSource で直す項目は `docs/operations/git-integration.md`「M3 で直すこと」の 4 つ。

### 0.5 R1 は本当は何で決まるか（前提の点検）

01 §8 の完了の条件は「forge 自身の開発を MR で回せる」と「復元の訓練に成功する」。ただしパイロットのチームは **kanflow を使うチーム**から選び（01 §11-8）、kanflow はプロジェクトごとに GitLab か GitStation の**一方だけ**を読む（02 §12、D-2・D-4）。したがって R1 の本当の条件は次の 2 つになる。

1. **パイロットのチームが使う本番の Kanflow が GitStationSource を持つこと。** GitStationSource は PG 化した Kanflow v2 の上に作るので、**R1 の前に本番の Kanflow の MySQL → PG の切り替え（人の承認）を終える必要がある**。代わりは「GitStationSource を MySQL の版にも入れる（二重の保守）」か「パイロットの間は開発タブを使わない運用」。**§10 Q2 で決めてほしい最大の判断。**
2. **GitLab への push ミラー**（06 D-2・D-4）。最初のチームが移る時点で Must。

逆に、Webhook（kanflow はイベントの履歴を引けば足りる）、F-36、リポジトリをまたぐ連携 (a)〜(c)、文脈 API と MCP、C-2、`spec-trace` は **R1 の条件ではない**ので M3 の後半に置く。

もう 1 つの前提: **M3 には CI が無い（M4）。** 「必須のチェック」を出せるのは、試験サーバーで動かすゲートのスクリプト（サービスアカウントで `gstn checks`）と kanflow だけ。forge 自身の `ci/gates` チェックはこのスクリプトで出す（§10 Q10）。

### 0.6 Windows の 61 件の失敗（製品の不具合ではなくテストの移植性）

2026-11-02 に Windows で全体のテストを回した結果: 1575 件中 1514 件合格・61 件失敗・1 件 skip。

| 件数 | 場所 | 原因 | 直し方（案） |
|---|---|---|---|
| 59 | forge-client | テストが `GSTN_CREDENTIAL_STORE=file` を使う。Windows では設計どおり拒否される | 保管方法を検査しないテストはテスト用の保管方法（feature `test-support` の注入）に替える。ファイルの保管のテストは `#[cfg(unix)]` にし、Windows では「拒否されること」を確かめるテストを足す（黙って飛ばさない） |
| 1 | forge-git `storage::adopt_hardening_tests::a_token_carried_by_an_include_is_refused` | Windows のパスをエスケープせずに git の config に書く | `git config --file` で書くか、`/` 区切りにする |
| 1 | forge-git `command::tests::revision_operand_selects_the_ref_in_real_git_log` | テストが作業ツリーのあるリポジトリの `.git` で `git log` を動かしたため、git がそのディレクトリを作業ツリーとみなし、loose ref のファイル `refs/heads/main` と曖昧になった | **テストだけを直した**（ベアの clone で読む）。本番のリポジトリは必ずベアで、git-protocol-reviewer が Windows の git 2.53 で「ベアでは起きない」ことを確かめた。revision の後に `--` を足すと `log` では revision がパスになるので製品は変えない |

**結果（P2、2026-11-02、`016e48f`）**: Windows で全体のテスト 1576 件すべて合格・1 件 skip。forge-client は `test-support` の機能の `test-file` の保管方法を使い（出荷する `gstn` に入らないことを CI で確認）、Windows で `file` を拒否するテストを足した。

## 1. M3 の範囲（R1 に必須か、後回しか）

| 項目 | 区分 | 理由・出典 |
|---|---|---|
| M3-0 の準備（Windows の移植性、試験サーバーで見つかった問題、CI の初回、試験環境、本物の Keycloak での通し） | **R1 必須** | 引き継ぎ §5 |
| MR の作成・差分・版・インラインコメント・スレッドの解決・ドラフト・衝突の検出（C-1） | **R1 必須** | 01 M3、04 C-1 |
| マージの方式（マージコミット・squash・早送りだけ） | **R1 必須** | 01 M3 |
| 「rebase して早送り」 | M3 後半 | §3.2 の案 B が要る。§10 Q3 |
| 承認のルール（数、承認できる人）、CODEOWNERS、保護ブランチのマージの条件（承認・必須チェック・スレッド・作業項目のリンク） | **R1 必須** | 01 M3、§5.2 |
| Checks API | **R1 必須** | 01 §5.2、02 §5.4 |
| 連携の土台（integrations・bindings・`work_items`・イベントの履歴 API・開発状況 API）とサービスアカウント | **R1 必須** | 02 §5.7・§7、GitStationSource の前提 |
| Webhook（署名・再送・配信の履歴） | M3（R1 の条件ではない。順序は利用者の指定どおり） | 02 §5.5、ADR-0016 §8 |
| 通知（メールの最小限: レビューの依頼・承認・マージ・ミラーの失敗） | **R1 必須（案）** | 01 M3。§10 Q6 |
| アプリ内の通知 | M3 後半 | — |
| 代理実行（ADR）と F-36 | M3 後半（パイロットに技術者でない承認者がいれば R1。§10 Q9） | 02 §5.3、ADR-0023 |
| **Kanflow v2 の PG 移行**と MySQL → PG のデータ移行の道具・リハーサル・runbook | **R1 必須**（本番の切り替えは承認された別の作業） | §0.5 |
| GitStationSource（provider `gitstation`） | **R1 必須** | §0.5 |
| バックアップと復元、復元の訓練 | **R1 必須** | 01 M3、§9 RPO 24h / RTO 4h |
| GitLab への push ミラー | **R1 必須** | 06 D-2・D-4 |
| リポジトリの保守（定期の gc / repack / fsck） | 復元の検証の fsck は R1、定期の実行は M3 後半 | 01 M3 |
| リポジトリをまたぐ連携 (a) → (b) → (c) | M3 後半 | 03 §8.1 |
| 構造化仕様、要件 ID の索引、`spec-lint`、`@req` の索引、`spec-trace`（助言） | M3 後半 | 03 §8、06 |
| 文脈 API と MCP（読み取り）、`gstn` の `context`・`pbi`・`spec`・`conflicts` | M3 後半 | 03 §6、06 |
| C-2（MR 同士の衝突の予測） | M3 後半 | 04 C-2 |
| 危険の検査の決定的な規則（`forge/risk`） | M3 後半（任意） | 04 M-6、§8 |
| push 時の検査（秘密の混入、大きいファイル） | M3 後半（Should） | 01 |
| フォーク | **M4 以降（案）** | 01 Should。社内はブランチで MR を出す運用が主。§10 Q8 |
| 2 要素認証 | **forge のコードは作らない**（Keycloak の設定。管理者の MFA は IoT 側 12 §7） | 01（2026-11-01） |
| kanflow のオーナーへのマージの能力、マージの予約、マージキュー、自動マージ | M4 | ADR-0017 決定 10 D・11、04 M-3・M-5 |

## 2. 依存の順

```
M3-0（A0 → P1 → P2 ∥ P3 ∥ P4 ∥ P5）
  └▶ MR の土台（R1→R2→R3→R4）─▶ 承認と保護ブランチ（R5→R6）
        └▶ Checks（C1）─▶ 連携の土台とイベントの履歴（I1）∥ サービスアカウント（S0）
              └▶ Webhook（I2）、通知（N1）
              └▶ 代理実行の ADR（S1）→ 代理実行（S2）
  ─▶ Kanflow: PG 移行（K1）→ データ移行の道具（K2）→ GitStationSource（K3）→ F-36（K4）
  ─▶ 運用: バックアップと復元（O1）、GitLab へのミラー（O2）、保守（O3）  ※ O1・O2 は R6 の後なら並行可
  ═▶ R1 の判定（§9）
  ─▶ 後半: 連携 (a) X1 → (b) X2 → (c) X3、仕様の索引と spec-trace（X4）、文脈 API と MCP（X5）、C-2（X6）、risk（X7）、push 時の検査（X8）
  画面: U1（R3 の後）→ U2（R6 の後）→ U3（I2・N1 の後）
```

Rust のビルドは同時に 2 つまで、全体のテストは 1 つずつ（引き継ぎ §3）。Kanflow の作業は別のリポジトリなので Rust のビルドと競合しない。

## 3. 設計の選択肢と推奨

### 3.1 MR のデータと差分の持ち方

| 軸 | **A: SHA だけを持ち、差分は git で毎回作る（推奨）** | B: 差分を DB に保存 | C: 差分をファイルの置き場に保存 |
|---|---|---|---|
| 内容 | `merge_requests` と `merge_request_versions`（push ごとの head・base・merge-base の SHA）。差分は M2 の `browse::diff` で作る。コミットが消えないよう隠した ref `refs/merge-requests/<iid>/head` を持つ | 版ごとに差分の行を DB に書く | 版ごとに差分の塊を保存 |
| 費用 | 小。既存の部品を使い回す | DB が膨らむ。git と DB の二重の真実 | 新しい保存先とそのバックアップの整合 |
| 失敗の見え方 | git の失敗は FG-GIT の既存のコード | DB とリポジトリのずれ | 復元時のずれ |
| 安全 | 隠した ref は upload-pack / receive-pack に見せない（`hideRefs`）。`git-protocol-reviewer` | — | — |

判断: **A**。差分の上限と `truncated` は M2 と同じ。

### 3.2 マージの実行（契約 5: git を実行する）

| 軸 | **A: 作業ツリー無しの plumbing（推奨）** | B: 一時的な作業ツリー（`git worktree` / `rebase`） | C: libgit2 など |
|---|---|---|---|
| 手順 | `merge-tree --write-tree` → `commit-tree`（マージコミットは親 2、squash は親 1）→ `update-ref <ref> <new> <old>`（期待値つきの比較と書き込み） | 一時ディレクトリで merge / rebase して update-ref | — |
| 方式 | マージコミット・squash・早送りだけ | + rebase | — |
| 危険 | 小。ディスクに作業ツリーを作らない | 一時ディレクトリの後始末、時間切れ、ディスク | **契約 5 に反するので不採用** |

判断: R1 は **A**（3 方式）。「rebase して早送り」は M3 後半に B で作るか、`git replay`（2.44 以上、まだ実験的）を使うかを決める（§10 Q3）。

マージは**ジョブで**実行する（`gstn mr merge --wait` は完了を待つだけ）。リポジトリごとの advisory lock を取り、ジョブの中で `authorize(MergeRequestMerge)` と `evaluate_merge()` を**ロックの下でもう一度**判定する（`forge-hosting` の reconfirm と同じ形）。head の SHA が判定に使ったものと違えば止める。update-ref は pre-receive のフックを通らないので、保護 ref の判定（ADR-0008 のポリシーのスナップショット）と push の副作用（push の記録、`repository.pushed`、`merge_request.merged`）はマージの経路から**同じトランザクション**で出す。

### 3.3 Checks API の形

| 軸 | **A: コミットの SHA に結ぶ check run（推奨、02 §5.4）** | B: 状態だけの commit status | C: MR に結ぶ |
|---|---|---|---|
| 内容 | `name`（名前空間 `ci/`・`forge/`・`kanflow/`・`ext/`）、`status`、`conclusion`、要約、`details_url`、注釈（上限つき） | 名前ごとに状態 1 つ | MR ごと |
| 将来 | M4 の CI と M5 の LLM がそのまま提供者になる（01 §5） | 注釈と助言を表せない | push で無効になり再利用できない |

判断: **A**。必須のチェックは「MR の head の SHA に、その名前の `success` がある」。チェックが無いことは成功と区別する（02 §8.3 O-6）。サービスアカウントは自分の名前空間にだけ書ける。

### 3.4 Webhook の配信

| 軸 | **A: Outbox の購読者 → 配信ごとのジョブ（推奨、ADR-0016 §8）** | B: 専用の配信の表と専用の worker | C: 要求の中で同期して送る |
|---|---|---|---|
| 理由 | 既存の `SKIP LOCKED` のジョブ・再試行・デッドレター・runbook `job-dead-lettered` を使い回す | 仕組みが二重 | 遅延と失敗が利用者の操作に漏れる |

判断: **A**。署名・許可リスト・自動の無効化は 02 §11.1 のとおり。

### 3.5 代理実行（F-36・(c) の結びつけで kanflow が本人として操作する）

| 軸 | A: サービスアカウント + `Forge-Act-As` だけ（02 §5.3 / ADR-0005） | **B: A + 状態を変える操作には、本人が forge で 1 回確認した主張を付ける（推奨）** | C: Keycloak のトークン交換（本人のトークン。IoT 12 §4・§11.5） |
|---|---|---|---|
| 判定 | サービスアカウントの範囲 ∩ 本人の権限 | 同左 + 承認には forge の確認の画面（SSO のセッション）で本人が押した、5 分・1 回限り・メソッド + パス + SHA に結んだ主張を要する（ADR-0017 決定 10 A、監査の `via_assertion_id`） | Keycloak が本人を保証 |
| 脅威（c）: 1 つのサービスアカウントが多くの人の承認を作る | **防げない** | 防げる | 防げる |
| 費用 | 小 | 中（小さな確認の画面、主張の表） | 大。IoT 側の Keycloak でトークン交換を有効にする必要があり、時期は**未確認** |
| 将来 | — | C や OAuth2 に替えられる形にする | — |

判断: **B**。主張を要するのは承認・承認の取り消し・結びつけの変更。読み取りは A だけでよい。代理で使える Action は許可リスト（§4.3）。ADR は MR-S1、`security-reviewer` 必須。

### 3.6 Kanflow v2 の PG への移行

| 軸 | **A: PG の最初のマイグレーションを作り直す（推奨）** | B: 58 本のマイグレーションを PG の方言に 1 本ずつ書き直す | C: マイグレーション無しの `prisma db push` |
|---|---|---|---|
| 根拠 | kanflow の計画 00 の行 100「移行のときに PG の最初のマイグレーションを作り直す」と一致 | 途中の版を PG で再現する必要は無い | 履歴を失う。本番の手順にならない |
| 作業 | MySQL の 58 本は `prisma/migrations-mysql/` に凍結して残す（本番の切り替えまで本番が使う）。PG の `0_init` + 手書きの SQL で GIN（`pg_trgm`）と `lower()` の一意の索引 | 大きい。誤りが混じりやすい | — |

移行で直すもの:
- `@@fulltext` 8 か所 → `pg_trgm` の GIN 索引と `contains` / `mode: "insensitive"`（検索を呼ぶ箇所の数は**未確認**。K1 の最初に数える）。
- `MediumText` → `text`。
- 大文字小文字を区別しない一意（メールなど）→ `lower()` の一意の索引か `citext`。
- 生の SQL 10 か所: `ON DUPLICATE KEY` → `ON CONFLICT`、バッククォート → 二重引用符、PascalCase の表名は引用符で囲む。
- `ops/backup/*.ps1` の mysqldump → pg_dump。

**データの移行**（本番の切り替えのため）:

| 案 | 内容 | 判断 |
|---|---|---|
| a: 自前の TS の道具（推奨） | Prisma が作った PG のスキーマに**データだけ**を FK の順に流す。表ごとに件数と内容のハッシュを照合、シーケンスを直す、乾式の実行、読み取り専用の MySQL のユーザー | 検証を自前で持てる |
| b: pgloader | 型の対応は楽。引用符つきの PascalCase と Prisma のスキーマとの一致の検証が弱い | 比較の対象 |
| c: mysqldump の変換 | 壊れやすい | 不採用 |

判断: **A と a**。

### 3.7 機能クレートの依存の向き（契約 9）

`forge-review`（MR・承認・`evaluate_merge`）は Checks の結果を要るが `forge-checks` に依存できない。`forge-core` に `CheckResultsSource` の trait を置き、`forge-web` / ジョブの実行側が 2 つを組み合わせる。`forge-integ`（連携・Webhook・ミラーのジョブ）と `forge-notify` はドメインイベント（Outbox）だけを読む。

## 4. 設計の要点

### 4.1 データ（すべて追記のマイグレーション。`tables.toml` に台帳の行、doctor の期待値も更新）

| マイグレーション（MR） | 表 |
|---|---|
| R1 | `merge_requests`（プロジェクトごとの `iid`、source / target、`state` = open / merged / closed、`draft`、head・base の SHA、`merge_status`）、`merge_request_versions`、MR の状態遷移の CHECK |
| R3 | `merge_request_discussions`、`merge_request_notes`（位置: 版・パス・行・側。古くなった行は `outdated`） |
| R4 | `merge_request_merges`（方式、マージした SHA、ジョブ）、プロジェクトの `merge_settings`（許可する方式） |
| R5 | `merge_request_approvals`（SHA に結ぶ。push で取り消す）、`approval_rules` |
| R6 | `protected_ref_merge_rules`（承認数、必須チェックの名前、スレッド、作業項目のリンク、直接の push の禁止） |
| C1 | `check_runs`、`check_annotations` |
| I1 | `integrations`、`integration_bindings`、`work_items`、`event_work_items`、`integration_feed_reads`（ADR-0016 §7） |
| I2 | `webhooks`、`webhook_deliveries` |
| S0 / S2 | サービスアカウントのトークンの範囲（プロジェクト・許可する操作）、`act_as_assertions` |
| N1 | `notification_preferences`、`notifications` |
| O2 | `push_mirrors`（宛先・資格情報のファイル名だけ・最後の成功・遅れ） |

### 4.2 イベント（02 §5.8 の MR の状態遷移の全量）

R1〜R6 で `merge_request.opened`・`draft_changed`・`pushed`・`updated`・`review_requested`・`approved`・`unapproved`・`commented`・`merged`・`closed`・`reopened`・`source_branch_deleted`・`conflict_detected`・`conflict_resolved` を出す。**状態遷移の一覧からテストを作り、イベントが出ない遷移があれば落とす**（02 §5.8 の約束）。`check.completed` は C1。

### 4.3 新しい Action と権限の表の行（どれも allow と deny の行。代理の列は S2 で足す）

| Action | allow の例 | deny の例 | 代理（S2） |
|---|---|---|---|
| MergeRequestCreate | source のブランチに push できる Developer 以上 | Reporter、private の Guest、匿名、削除待ちのプロジェクト | 不可（M4 で F-2 と一緒に） |
| MergeRequestRead / List | `RepoBrowse` と同じ | 同左 | 読み取りは可 |
| MergeRequestUpdate / Close / Reopen | 作成者、Developer 以上 | Reporter、他人のドラフトの変更 | 不可 |
| MergeRequestComment | Reporter 以上 | private の Guest、匿名、エージェントのトークン（案） | 不可 |
| DiscussionResolve | MR の作成者、スレッドを始めた人、Developer 以上 | Reporter | 不可 |
| MergeRequestApprove / Unapprove | ルールで承認できる人 | **作成者自身**、エージェントのトークン（M3）、ルールで承認できない人 | **可（主張が必須）** |
| MergeRequestMerge | target の `push_min_role` 以上かつ `evaluate_merge` が通る | 条件未達、エージェント、ドラフト | 不可（M4） |
| MergeSettingsManage / ApprovalRuleManage | Maintainer 以上 | Developer | 不可 |
| CheckWrite / CheckRead | 自分の名前空間を持つサービスアカウント、`ext/` は api スコープの Developer 以上 / `RepoBrowse` と同じ | 他の名前空間（`forge/` は system だけ） | 不可 |
| WebhookManage / DeliveryRead / Redeliver | Maintainer 以上 | Developer、許可リスト外の宛先 | 不可 |
| IntegrationManage | 管理者 | 管理者以外 | 不可 |
| IntegrationBindingManage | 対象リポジトリの Maintainer 以上 | Developer、コードを持つリポジトリを 2 つ目の kanflow のプロジェクトに結ぶ | **可（主張が必須）** |
| IntegrationEventRead | その連携のサービスアカウント | 他の連携、**代理での読み取り**（ADR-0016 §7） | 不可 |
| WorkItemDevStatusRead / WorkItemLink | Reporter 以上 / Developer 以上 | 結んでいないプロジェクト | 読み取りは可 |
| ServiceAccountManage / ServiceAccountTokenIssue | 管理者 | 管理者以外、トークンでのトークンの発行 | 不可 |
| NotificationRead / PreferenceManage | 本人 | 他人 | 不可 |
| PushMirrorManage | 管理者（案。資格情報を扱う） | Maintainer | 不可 |

`forge backup` / `forge restore` は HTTP から届かない運用者の CLI（`system`）。**ADR-0019 §8 の例外の一覧に足す**（ADR の Amendment、`Git/CLAUDE.md` の契約 4 の括弧も直す）。エージェントモデルの `authz-coverage` ゲートが `DECIDED_IN_ROUTE`（`decided_in`）を理解するのが前提（A0）。

### 4.4 エラーコード（番号は登録時の次の空き。INT は 02 §11.3 の予定の番号）

| コード（仮） | 名前 | HTTP | 重さ | runbook |
|---|---|---|---|---|
| FG-REVIEW-0001 | MergeRequestConflict（衝突でマージできない） | 409 | info | — |
| FG-REVIEW-0002 | MergeBlocked（条件未達。理由の一覧を `reason` に） | 409 | info | — |
| FG-REVIEW-0003 | HeadChanged（判定の後に head が変わった） | 409 | info | — |
| FG-REVIEW-0004 | MergeExecutionFailed（CAS の負け以外の git の失敗） | 500 | **alert** | `merge-execution` |
| FG-REVIEW-0005 | DuplicateOpenMergeRequest | 409 | info | — |
| FG-REVIEW-0006 | SelfApprovalRefused | 403 | info | — |
| FG-REVIEW-0007 | CodeownersInvalid（target のブランチのファイルを解析できない。黙って無視せず条件未達にする） | 422 | warn | `codeowners` |
| FG-REVIEW-0004（R3 で登録。番号は登録の順、§11.4） | DiscussionPositionInvalid | 422 | info | — |
| FG-REVIEW-0009 | MergeMethodNotAllowed | 409 | info | — |
| FG-REVIEW-0010 | CheckNamespaceDenied | 403 | info | — |
| FG-REVIEW-0011 | CheckPayloadInvalid（注釈の上限超えなど） | 422 | info | — |
| FG-INT-0001〜0012 | 02 §9・§11.3 のとおり（0004 デッドレター、0006 代理の相手、0007 410、0008 カーソル、0009 読みに来ない、0010 宛先、0011 自動の無効化、0012 上限） | — | 0009 / 0011 は warn | `integration-feed-lag`、`webhook-delivery` |
| FG-INT-0013 | PushMirrorFailed | — | **alert** | `gitlab-mirror` |
| FG-INT-0014 | PushMirrorLagging（遅れがしきい値超え） | — | **alert** | `gitlab-mirror` |
| FG-INT-0015 | SmtpUnavailable | — | warn | `notification-email` |
| FG-AUTH-0019 | ActAsAssertionInvalid（期限切れ・使用済み・別のメソッド / パス / SHA） | 403 | warn | `act-as` |
| FG-SYS-0008 | BackupFailed | — | **alert** | `backup-restore` |
| FG-SYS-0009 | RestoreVerificationFailed（DB とリポジトリの不整合、fsck の失敗） | — | **alert** | `backup-restore` |
| FG-GIT-next | RepositoryIntegrityFailed（定期の fsck） | — | **alert** | `repo-storage`（更新） |

権限の拒否は FG-AUTH-0003、見えない MR は M2 の「認可の後でだけ返す 404」の形、git のプロセスの失敗は FG-GIT の既存のコード、DB は FG-DB-0001 を使い回す。

## 5. MR の分け方

- 置き場所: **[root]** = エージェントモデルのリポジトリ（先に入れる）、**[KF]** = Kanflow のリポジトリ、印なし = GitStation。
- ブランチの area は既存の語（`core`・`git`・`ci`・`auth`・`web`・`ui`・`ops`・`cross`・`docs`）を使う（`review` は area に無い）。
- 各 MR の終わり: fmt / clippy / nextest、`gen-catalog`・`gen-authz` の差分なし、web は lint・typecheck・test・build・i18n・journey、`/review` で CRITICAL / HIGH なし（再レビューは指摘した評価役が修正の差分だけ、最大 2 回）、引き継ぎを書く。
- `gstn` のコマンドはルートと**同じ MR** で足す。どのルートも C-1 の表（`forge-client/src/coverage.rs`）に行が要る。

### 5.1 M3-0 の準備

| MR | 中身 | 先に書くテスト | 評価役 | モデル |
|---|---|---|---|---|
| **A0 [root]** `chore/harness-m3-authz-coverage` | `authz-coverage` のゲートに `DECIDED_IN_ROUTE`（`decided_in`）と M3 の Action を教える。Kanflow のリポジトリを評価役に見せる手順をルーティングに書く（`typescript-reviewer`・`database-reviewer` が `Git/**` の外を読めるかは**未確認**） | ゲートの eval（`decided_in` の行が無いと落ちる / あると通る） | `harness-optimizer` | Sonnet |
| P1 `docs/docs-m3-design` | この計画、**ADR-0024**（MR・版・マージの実行・Checks・`evaluate_merge`・隠した ref）、ADR-0019 §8 の Amendment（backup / restore）、01 §8 の R1 の分け方、02 §5.3・§8.1 の改訂（§0.3）、03 §8 の表の修正 | なし | `security-reviewer`、`git-protocol-reviewer`、`authz-matrix-auditor`（設計として） | **Opus** |
| **P6** `fix/git-browse-reachability` **（R1 必須。R1a の前に入れる）** | 設計レビュー（2026-11-02）の HIGH（ADR-0024 末尾の表の 25）: 閲覧の API の SHA の指定（`browse/refs.rs` の `commit_of`）に到達可能性の検査を足す。ブランチ・タグ（MR の文脈では、その MR の版の ref）から届かないコミットは、存在しない SHA と同じ応答（FG-REPO-0023 `ref_not_found`）。確認は時間切れつき（時間切れは FG-GIT-0002 / 空きなしは FG-GIT-0004 で、届いたことにしない）、呼び出し元ごとの流量制限、`(repo, sha, refs の世代)` の短いキャッシュ、commit-graph があれば使う（ADR-0024 §2）。版の ref の削除（`MergeRequestVersionRefsPurge`、人だけ・`SessionOnly`・監査つき）は R1b で足す | 実 git: 削除したブランチだけのコミット・`commit-tree` が作っただけのコミットが ID で読めない（`unreachable_commit_is_not_browsable_by_id`）、ブランチ・タグから届くコミットは読める、時間切れが成功にならない、流量制限、ref の更新でキャッシュが無効になる、**性能のテスト**（多数のブランチ・深い履歴で閲覧の時間切れの内） | `git-protocol-reviewer`、`security-reviewer`、`rust-reviewer` | **Opus** |
| P2 `test/cross-windows-portability` **（完了 `016e48f`）** | §0.6 の 3 つ（テストだけの修正。製品のコードは変えていない） | Windows と Linux の両方で: `revision_without_paths_is_separated_by_double_dash`、`file_credential_store_is_refused_on_windows`、include のテストのパスの書き方 | `rust-reviewer`、`git-protocol-reviewer`、`pr-test-analyzer` | Sonnet |
| P3 `fix/ops-test-server-findings` | 試験サーバーの §9 の 8（OIDC 以外で 2 人目の利用者を作る runbook か、`UserCreate` を PLANNED から外すかは利用者の答え次第）と 9（`forge admin` の DB ロールの runbook とコードを揃える）。利用者から受け取った残りの結果 | 見つかった問題ごとに失敗するテストを先に | `rust-reviewer`、`silent-failure-hunter`、該当する専門の評価役 | Sonnet |
| P4 `ci/ci-first-run` | GitHub Actions を `main` で初めて回し、赤を直す。npm のミラーの答え（ADR-0022 §8）の反映 | CI の緑が証拠 | `security-reviewer`（取り寄せ先） | Sonnet |
| P5 `chore/ops-m3-test-environment` | §7 の試験環境の compose（GitStation・Keycloak は PG）、合成の realm（config-as-code）。**本物の Keycloak での通し**（ログイン → ログアウト → 無効化が次の操作から効く。M2 の持ち越し） | Playwright: `keycloak_disable_takes_effect_on_next_request` | `security-reviewer`、`typescript-reviewer`（E2E）、`e2e-runner` | Sonnet |

### 5.2 MR の土台（R1）

| MR | 中身 | 先に書くテスト | エラーコード | 評価役 | モデル |
|---|---|---|---|---|---|
| R1 `feat/core-merge-requests` | クレート `forge-review`。MR の作成・一覧・表示・更新・クローズ・再オープン・ドラフト、`iid` の採番（原子的）、隠した ref、push で版を足す（`repository.pushed` の購読者）、状態遷移のイベント。`gstn mr create/list/view` | 状態遷移の一覧からイベントのテスト、`iid` の同時採番、権限の表（§4.3）、ブランチの削除後も MR とリンクが残る、隠した ref が clone に出ない（実 git） | REVIEW-0005 | `rust-reviewer`、`database-reviewer`、`authz-matrix-auditor`、`git-protocol-reviewer`、`error-contract-auditor` | **Opus** |
| R2 `feat/git-merge-request-diff-mergeability` | 版の差分・版どうしの比較。C-1: `merge-tree --write-tree` のジョブ（MR の更新と target の前進で）、`conflict_detected/resolved`。`gstn mr diff` | 衝突のファイルと箇所、判定できないときは理由のコードつき（成功にしない）、時間切れ、大きい差分の `truncated` | REVIEW-0001、GIT の既存 | `git-protocol-reviewer`、`rust-reviewer`、`silent-failure-hunter`、`pr-test-analyzer` | **Opus** |
| R3 `feat/core-merge-request-discussions` | コメント・インライン・スレッドの解決・古くなった位置。`gstn mr comment` | 古い版の行への位置、`outdated`、権限の行 | REVIEW-0008 | `rust-reviewer`、`database-reviewer`、`authz-matrix-auditor`、`pr-test-analyzer` | Sonnet |
| R4 `feat/git-merge-execution` | §3.2 の A（3 方式）、ジョブ、ロックの下の reconfirm、update-ref の CAS、副作用のイベント、ソースブランチの削除（任意）。`gstn mr merge --wait`。**ref を書き換えるすべての経路（マージの実行、ソースブランチの削除）は、そのリポジトリの `Browser::invalidate` を呼ぶ**（閲覧の ref 一覧と P6 の到達のキャッシュを捨てる。P6 のレビュー） | 実 git: 3 方式の結果の木、同時の push で CAS の負け → REVIEW-0003、`-` で始まる ref、時間切れの後始末、保護 ref の判定がマージの経路でも効く、**マージ・ブランチ削除の直後の閲覧が古い答えを返さない（`invalidate` を呼ぶことのテスト）** | REVIEW-0003・0004・0009 | `git-protocol-reviewer`、`security-reviewer`、`rust-reviewer`、`silent-failure-hunter`、`authz-matrix-auditor`、`error-contract-auditor` | **Opus** |
| U1 `feat/ui-merge-requests` | MR の一覧・作成・概要（**止まっている理由を 1 行**、08）・差分・インラインコメント | vitest + MSW、Playwright（作成 → コメント → 解決）、axe | — | `typescript-reviewer`、`a11y-architect`、`i18n-sync-auditor`、`design-reviewer`、`e2e-runner` | Sonnet |

runbook: `merge-execution`（新規）。

### 5.3 承認と保護ブランチ（R1）

| MR | 中身 | 先に書くテスト | エラーコード | 評価役 | モデル |
|---|---|---|---|---|---|
| R5 `feat/core-approval-rules-codeowners` | 承認・取り消し（SHA に結ぶ。新しいコミットで消すのが既定、Q4）、ルール（数・承認できる人）、CODEOWNERS（target のブランチから読む。文法は Q5）、自分の MR の承認の拒否 | CODEOWNERS の各文法と不正なファイル、push で承認が消える、自己承認の拒否、エージェントの拒否、ルールの変更で `evaluate_merge` が変わる | REVIEW-0013〜0015（登録の順に確定。計画の 0006・0007 は仮） | `rust-reviewer`、`authz-matrix-auditor`、`security-reviewer`、`database-reviewer`、`error-contract-auditor` | **Opus** |
| R6 `feat/core-merge-conditions` | （R5 から）`merge_settings` と `codeowners_enforced` の期限つきの切り替え（`CodeownersEnforcementToggle`、直接の push の禁止より先）、`reset_approvals_on_push`、`protected_ref_merge_rules`、`evaluate_merge()`（純粋な関数。止める理由の一覧を返す）、保護ブランチへの直接の push の禁止の設定、`gstn mr status --json` | 条件の組み合わせの表（proptest）、ドラフト、チェックが無いことは成功と区別、ジョブの中での再判定 | REVIEW-0016、GIT-0017（登録の順に確定。計画の 0002 は仮） | `rust-reviewer`、`authz-matrix-auditor`、`git-protocol-reviewer`、`pr-test-analyzer` | **Opus** |
| U2 `feat/ui-merge-approve-settings` | 承認・マージのボタン、条件のパネル、プロジェクトの設定（方式・ルール・必須チェック） | 理由の 1 行の表示、CSRF、E2E | — | U1 と同じ | Sonnet |

### 5.4 Checks・連携・Webhook・通知・サービスアカウント

| MR | 中身 | 先に書くテスト | エラーコード | 評価役 | モデル | R1 |
|---|---|---|---|---|---|---|
| C1 `feat/ci-checks-api` | クレート `forge-checks`、02 §5.4、名前空間、結論が変わったときだけ `check.completed`、`CheckResultsSource` の trait。`gstn checks` | 名前空間の拒否、注釈の上限、同名の再登録、必須チェックとのつながり | REVIEW-0010・0011 | `rust-reviewer`、`authz-matrix-auditor`、`database-reviewer`、`error-contract-auditor` | **Opus** | ○ |
| S0 `feat/auth-service-accounts` | サービスアカウントの作成（管理者）、範囲（プロジェクト・操作）つきのトークン、`user_email` のスコープ | 範囲外のトークンの拒否、無効化したアカウント、人間のメンバーになれない（既存のトリガー） | AUTH の既存 | `security-reviewer`、`authz-matrix-auditor`、`database-reviewer`、`rust-reviewer` | **Opus** | ○ |
| I1 `feat/cross-integration-event-feed` | クレート `forge-integ`、integrations・bindings、PBI の参照の検出（kanflow の K1 と同じ正規表現とテストのベクタ）、`work_items`、イベントの履歴 API（AEAD のカーソル、`prove_visible`、`/cursor`）、開発状況 API（`state=all`）。`gstn pbi` | **02 §8.2 の回帰テスト 5 つ**、410 の 4 つの `reason`、他の連携のカーソル、解除後は新旧どちらのイベントも返さない、代理での読み取りの拒否 | INT-0007・0008・0009・0012 | `security-reviewer`、`authz-matrix-auditor`、`database-reviewer`、`rust-reviewer`、`silent-failure-hunter` | **Opus** | ○ |
| N1 `feat/cross-notifications` | クレート `forge-notify`、メール（社内の SMTP、時間切れ）、利用者ごとの設定（アプリ内は後半） | SMTP の時間切れで warn と再試行、本文に内部の情報を入れない、設定の権限 | INT-0015 | `rust-reviewer`、`silent-failure-hunter`、`security-reviewer` | Sonnet | ○（Q6） |
| I2 `feat/cross-webhooks` | ADR-0016 §8 のとおり（署名・許可リスト・再送・自動の無効化・配信の履歴）。`gstn webhook` | SSRF（ループバック・リンクローカル・userinfo・リダイレクト）、署名の鍵の入れ替えの 24 時間、50 回連続の失敗で無効 | INT-0004・0010・0011 | `security-reviewer`、`silent-failure-hunter`、`authz-matrix-auditor`、`rust-reviewer` | **Opus** | — |
| S1 `docs/docs-adr-act-as` | **ADR-0025**（§3.5 の B、相手の指し方、許可リスト、主張、監査の列、OAuth2 / トークン交換への移行） | なし | — | **`security-reviewer`（必須）**、`authz-matrix-auditor` | **Opus** | — |
| S2 `feat/auth-act-as` | ヘッダーの解析、`Principal` の代理の形、範囲 ∩ 本人、主張の発行（forge の確認の画面）と 1 回の使用、監査、HMAC の鍵 | 許可リスト外の Action、相手が無効・人間でない、主張の再使用・期限切れ・別の SHA、ヘッダー 2 つ、範囲外のプロジェクト | INT-0006、AUTH-0019 | `security-reviewer`、`authz-matrix-auditor`、`rust-reviewer`、`error-contract-auditor`、`silent-failure-hunter` | **Opus** | — |
| U3 `feat/ui-webhooks-notifications-confirm` | Webhook の設定と配信の履歴、通知の設定、代理の確認の画面 | E2E、a11y | — | U1 と同じ + `security-reviewer`（確認の画面） | Sonnet | — |

runbook: `integration-feed-lag`、`webhook-delivery`、`notification-email`、`act-as`（新規）。

### 5.5 運用（R1）

| MR | 中身 | 先に書くテスト | エラーコード | 評価役 | モデル |
|---|---|---|---|---|---|
| O1 `feat/ops-backup-restore` | `forge backup`（DB を先、その後でリポジトリ。道具は Q11）と `forge restore verify`（DB の ref の期待値とリポジトリの比較、`fsck --connectivity-only`、Outbox の `epoch` を進めて連携に 410 `epoch_mismatch`）。doctor に「最後の成功からの時間」。**復元の訓練の runbook**。**復元は失効した資格情報と外した権限を生き返らせない**（復元の前に発行されたすべての資格情報 — セッション・個人のトークン・サービスアカウントのトークン・連携の秘密 — を失効し、再発行の手順を runbook に書く。バックアップの外の監査の写しから、無効化・所属の削除・ロールの引き下げ・管理者の印の削除・保護 ref の強化を当て直す。写しが無ければ管理者の印をすべて外して一覧に出す）。**バックアップは暗号化し、ファイルの権限を絞る**（ADR-0019 の Amendment 計画 11 P1、ADR-0024 末尾の表の 26）。版の ref（`refs/merge-requests/*`）を含め、`verify` は `merge_request_versions` と突き合わせる | 合成データで: バックアップ → 消す → 戻す → 検証、DB だけ古いときの検出、復元後に GitStationSource が 410 から立ち直る、`restore_does_not_resurrect_revoked_credentials`、`restore_reapplies_post_backup_role_changes`、権限の広い保存先を断る | SYS-0008・0009 | `database-reviewer`、`git-protocol-reviewer`、`silent-failure-hunter`、`rust-reviewer`、`error-contract-auditor` | **Opus** |
| O2 `feat/git-gitlab-push-mirror` | `repository.pushed` を受けるジョブでブランチとタグを GitLab へ送る（明示の refspec `refs/heads/*`・`refs/tags/*` だけで `refs/merge-requests/*` を送らない。ADR-0024 Risks、引数のベクタ、資格情報はファイルから credential helper で渡し URL に書かない、時間切れ、削除も送る）、遅れのメトリクスとアラート、切り替えの runbook | テストの宛先はローカルのベアのリポジトリ: 送る、削除の反映、宛先の拒否 → 0013、遅れ → 0014、資格情報がログに出ない | INT-0013・0014 | `git-protocol-reviewer`、`security-reviewer`、`silent-failure-hunter`、`rust-reviewer` | **Opus** |
| O3 `feat/git-repo-maintenance` | 定期の gc / repack / fsck のジョブ（同時実行の上限、push との排他）。到達できないオブジェクトの prune は猶予を明示（`--prune=<猶予>`、照合の窓とジョブの再試行の全体より長く、既定 24 時間以上。ADR-0024 §3） | fsck の失敗でアラート、push 中に gc を始めない、猶予の内の参照されないオブジェクト（照合の途中の `new_sha`）が消えない、**版の ref（`refs/merge-requests/<iid>/v<n>`）だけが指すコミットが gc の後も残る**。C1 のレビュー（2026-11-03）: **チェックの実行の保持** も範囲に入れる — `updated_at` が 180 日より古く、開いている MR の今の head でないコミットの `check_runs` と `check_annotations` を消す（監査の記録は残す。ADR-0024「C1 の独立レビュー」） | GIT-next | `git-protocol-reviewer`、`rust-reviewer`、`silent-failure-hunter` | Sonnet |

runbook: `backup-restore`（訓練の手順を含む）、`gitlab-mirror`（遅れ・失敗・GitLab への切り替え・戻し）、`repo-storage` の更新。

### 5.6 Kanflow 側 [KF]（Kanflow のリポジトリの規則とブランチ名に従う。GitStation とは別のコミット・MR）

| MR | 中身 | 先に書くテスト | 評価役 | R1 |
|---|---|---|---|---|
| K1 [KF] `feat/L4-db-postgresql` | §3.6 の A。provider `postgresql`、`0_init`、`pg_trgm` と `lower()` の索引、生の SQL 10 か所、`contains` の `mode: "insensitive"`、バックアップのスクリプト、`.env.example`、試験用インスタンスの compose（PG） | 既存の DB のテストを PG で全部通す、大文字小文字を区別しない検索とメールの一意、`FOR UPDATE` 3 か所の排他、`SKIP LOCKED` の worker | `typescript-reviewer`、`database-reviewer`、`silent-failure-hunter` | ○ |
| K2 [KF] `feat/L4-db-mysql-to-pg-migrator` | §3.6 の a（データだけ、件数とハッシュの照合、乾式の実行、読み取り専用のユーザー、本番の PG を宛先に取らない防御）、**リハーサルの記録と切り替えの runbook**（手順はすべて `[MUTATING — needs approval]`、戻し方つき） | 合成の MySQL → PG で照合が一致、1 行壊すと照合が落ちる、途中で止めてもやり直せる | `database-reviewer`、`typescript-reviewer`、`security-reviewer`（資格情報） | ○（本番の切り替えは別） |
| K3 [KF] `feat/L4-git-gitstation-source` | `DevSource` の実装（カーソル `gitstation:v1:<forge のカーソル>`、`restart()` は `/cursor` と開発状況 API）、`GIT_PROVIDERS` に `gitstation`、git-integration.md「M3 で直すこと」の 4 項目 | `dev-source-contract.ts` を偽の GitStation と試験用インスタンスの両方で、410 の 4 つの理由、クローズした MR が出る（02 §8.2） | `typescript-reviewer`、`security-reviewer`、`silent-failure-hunter` | ○ |
| K4 [KF] `feat/L4-git-mr-approve` | F-36: PBI の画面と「自分のタスク」に承認・取り消しのボタン、確認の画面を開いて主張を受け取る、承認数の表示（開発状況 API から） | 主張なしの送信は拒否、権限の無い人にはボタンを出さない（理由つき）、二重押し | `typescript-reviewer`、`security-reviewer`、`a11y-architect` | Q9 |

### 5.7 M3 後半

| MR | 中身 | 評価役 |
|---|---|---|
| X1 `feat/core-repo-yml-spec-refs` | (a) `.forge/repo.yml` の解析と参照先の版（タグ）の解決、参照の権限は読む人の権限で判定 | `rust-reviewer`、`authz-matrix-auditor`、`security-reviewer`、`error-contract-auditor` |
| X2 `feat/ci-spec-drift-check` | (b) `forge/spec-drift`（`neutral`、決定的） | `rust-reviewer`、`pr-test-analyzer` |
| X3 `feat/cross-multi-repo-bindings` + [KF] `feat/L4-git-multi-repo-binding` | (c) 03 §7.1、代理実行で結ぶ、コードを持つリポジトリを 2 つ目のプロジェクトに結ぶのは拒否 | `security-reviewer`、`authz-matrix-auditor`、`typescript-reviewer` |
| X4 `feat/core-spec-index-trace` | 要件ブロックの解析・ID の索引・`spec-lint`・`@req` の索引・`spec-trace`（助言。`PBI-123/AC-2` も）・仕様の API（03 §7.2）。`gstn spec` | `rust-reviewer`、`database-reviewer`、`pr-test-analyzer` |
| X5 `feat/cross-context-api-mcp` | 文脈 API（03 §6）、`gstn context` と MCP サーバー（読み取り）。D-5 の機密の PBI はエージェントのトークンに返さない | `security-reviewer`、`authz-matrix-auditor`、`rust-reviewer` |
| X6 `feat/git-conflict-prediction` | C-2（変更ファイルの重なりで絞ってから `merge-tree`、ドラフトも対象）、`merge_request.conflict_predicted`、`gstn conflicts` | `git-protocol-reviewer`、`rust-reviewer`、`silent-failure-hunter` |
| X7 `feat/ci-risk-deterministic` | 04 M-6 の決定的な規則、`gstn risk check`（任意） | `security-reviewer`、`rust-reviewer` |
| X8 `feat/git-push-checks` | push 時の秘密と大きいファイルの検査（Should） | `git-protocol-reviewer`、`security-reviewer` |
| U4 `feat/ui-in-app-notifications` | アプリ内の通知 | UI の 5 つ |

## 6. 運用で足すもの（まとめ）

- **メトリクス**: マージのジョブの時間と失敗、マージ可否の判定の遅れ、Webhook の配信の結果、イベントの履歴を読みに来ない時間、ミラーの遅れ（秒）、最後のバックアップからの時間、fsck の結果。
- **`forge doctor`**: `mirror.*`（宛先・遅れ・資格情報のファイル）、`backup.last_success`、`smtp`、`webhook.allowed_targets`、`act_as.keys`、`jobs.merge`。
- **ログ**: すべての失敗に `request_id` と `error_code`。代理の操作は `actor.via` を必ず出す。

## 7. 試験環境（本番の Kanflow には触れない）

すべて `timescale/timescaledb:2.28.1-pg16`、合成データだけ。ポートは 2026-11-02 に `netstat -ano` で空きを確かめた（このホストでは 3000・3100・3200・8080・15432〜15434・15442・15445・18025・18080 が他のサービスで使用中。55432 = nextest の DB、58080 = dev の Keycloak）。

| 部品 | 置き方 | ポート（案） | DB |
|---|---|---|---|
| GitStation の試験用インスタンス | `main` を release でビルドし `forge serve` + worker | 127.0.0.1:28080 | 専用 PG `gs-test-pg` 127.0.0.1:25432（**nextest のクラスタ 55432 とは別**。復元の訓練もここ） |
| Keycloak の試験 realm | `quay.io/keycloak/keycloak:26.4.2` を `start`（start-dev ではない）、`KC_DB=postgres`。realm `forge-m3-test`（client `gitstation`・`kanflow-test`、ロール `access`、社員番号の mapper、合成の利用者）を config-as-code で（IoT 12 §11.2） | 127.0.0.1:28180 | 専用 PG `kc-test-pg` 25433 |
| Kanflow v2 の試験用インスタンス | Kanflow のリポジトリの git worktree（v2 のブランチ）、`PORT=3300`、Keycloak `kanflow-test` | 127.0.0.1:3300 | 専用 PG `kf-test-pg` 25434 |
| MySQL のリハーサル用（K2） | 合成データの MySQL 8.4 コンテナ（本番の MySQL ではない） | 127.0.0.1:23306 | — |
| メールの受け口（N1） | 社内で動かす SMTP の受け口（例: mailpit） | 127.0.0.1:28025 | — |
| ミラーの宛先 | nextest はローカルのベアのリポジトリ。試験環境は社内 GitLab の使い捨てのプロジェクト（Q7） | — | — |

守ること:
- Kanflow の移行の道具は、宛先の PG と元の MySQL を明示した引数でだけ動く。元の MySQL は読み取り専用のユーザーで読む。本番の :3000 の `DATABASE_URL` を既定値として読まない。
- 本番の DB の複写を使うリハーサルは、利用者の承認があるときだけ社内の環境で行う（Q12）。
- compose は GitStation の `ops/test-env/compose.yml` と Kanflow 側の同等のファイルに置く。

## 8. 危険と対策

| 危険 | 対策 |
|---|---|
| 範囲が大きい（約 35 本） | §1 の R1 の区分を守る。R1 の判定は §9 の条件だけ。後半は R1 の後でもよい |
| R1 が本番の Kanflow の切り替えに縛られる | K1・K2 を早く終え、リハーサルを繰り返す。Q2 で時期を決める |
| v2 の PG 化の後、本番の Kanflow（MySQL）に新機能を出せない | 切り替えまで本番向けは MySQL の修正だけの系統（Q13） |
| マージの経路が保護 ref の判定を迂回する | update-ref の前にロックの下で reconfirm、マージの経路でも保護のテスト（R4） |
| 代理実行が権限の昇格の経路になる | §3.5 の B、許可リスト、ADR と `security-reviewer` 必須 |
| Webhook の SSRF | 許可リストは既定で空、常に拒否する範囲、リダイレクトに従わない |
| CI が無く必須チェックの提供者がいない | 試験サーバーのゲートのスクリプトがサービスアカウントで `ci/gates` を出す（Q10） |
| 復元で DB とリポジトリがずれる | DB → リポジトリの順、`restore verify`、`epoch` の 410 で連携が取り直す |
| Windows で全体のテストを信用できない | P2 を最初に入れる |
| Kanflow の差分を見る評価役の範囲 | A0 で確かめる。届かなければ Kanflow のリポジトリの規則で見る |

## 9. 完了の条件

**M3（R1 の判定）**
1. **forge 自身の開発を MR で回せる**: 試験用インスタンスで `main` を保護（MR 必須、承認 1、必須チェック `ci/gates`、スレッドの解決）し、M3 の最後の MR を forge の MR でマージする。そのマージが GitLab へ遅れなくミラーされる（遅れのアラートが鳴らない）。
2. **復元の訓練に成功する**: 試験用インスタンスで、バックアップ → 消す → 戻す → `forge restore verify` が通り、RTO（4 時間以内）を測って記録する。Kanflow v2 の GitStationSource が 410 `epoch_mismatch` から立ち直る。
3. Kanflow v2 の試験用インスタンスが GitStationSource で GitStation を読み、02 §8.2 の回帰テストが通る。
4. 本物の Keycloak での通し（ログイン・ログアウト・無効化が次の操作から効く）。
5. 新しいルートはすべて権限の表と C-1 の表にある。エラーのカタログ・runbook・ADR-0024 / 0025 がそろう。Windows でも全体のテストが緑。
6. **O3（保守の prune）が入っている**（R2 のレビューで追加、2026-11-02）: R2 のマージできるかの判定は判定のたびに参照されない木・blob を
   書き、forge のリポジトリは `gc.auto=0` なので、猶予つきの prune（O3）が無いとオブジェクトが増え続ける（秘密の除去の後も判定の残りが残る）。
   O3 がマージされるまで M3 を完了にしない。それまでは runbook `merge-request-mergeability` の手での `git prune --expire=<猶予>`（承認つき）。
7. **R1 を始める条件**（M3 の外。人の決定と承認）: パイロットのチームの決定、本番の Kanflow の PG への切り替えを承認された runbook で終えること（Q2 で案 a の場合）。

**確かめのコマンド**: `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`FORGE_TEST_DATABASE_URL=… cargo nextest run --workspace -j 4`、`gen-catalog` / `gen-authz` の差分なし、web の 6 つのゲート、`node scripts/self-heal.js --check`。Kanflow は自分のリポジトリのゲート（lint・typecheck・test、PG の DB のテスト）。

## 10. 利用者に決めてほしいこと（推奨つき）

1. **R1 のパイロットのチーム**（01 §11-8 で未定）。推奨: kanflow を使っていて、GitLab の MR の運用に慣れた 1 チーム。技術者でない承認者がいるかで Q9 が変わる。
2. **本番の Kanflow を PG に切り替える時期と R1 との関係**（最重要）。案 a: R1 の前に K2 のリハーサルを経て承認を得て切り替える（推奨）。案 b: GitStationSource を MySQL の系統にも入れる。案 c: パイロットの間は開発タブを使わない。
3. **マージの方式の既定**。推奨: マージコミットと squash を許可、既定はマージコミット。「rebase して早送り」は M3 後半（作業ツリーの B）か M4 か。
4. **承認の既定**。推奨: 保護ブランチは承認 1、作成者は自分の MR を承認できない、新しいコミットで承認を消す、エージェントのトークンは承認できない（M3）。
5. **CODEOWNERS の文法**。推奨: GitLab と同じ形の一部（パスの glob と `@user` / `@group`。セクションと任意の承認は入れない）。置き場所は `.forge/CODEOWNERS`、無ければ `CODEOWNERS`。
6. **通知**: 社内の SMTP（ホスト、送信元、認証）はあるか。推奨: R1 はメールの最小限、アプリ内は後半。
7. **GitLab へのミラー**: 宛先のグループと名前の規則、GitLab 側の資格情報（プロジェクトのアクセストークン）、ミラー側の保護ブランチで強制 push を許すか、遅れのしきい値（推奨 5 分）。
8. **2 要素認証とフォークを M3 に残すか**。推奨: 2 要素認証は Keycloak の設定だけで forge のコードは作らない（管理者の MFA は IoT 側に依頼）。フォークは M4 以降、需要を見て。
9. **F-36 を R1 に入れるか、承認に forge の確認の画面を挟むか**。推奨: 確認の画面を挟む（§3.5 の B、ADR-0017 決定 10 A と同じ）。R1 に入れるのはパイロットに技術者でない承認者がいる場合だけ。
10. **M4 の CI までの必須チェックの提供者**。推奨: 試験サーバーで fmt / clippy / nextest を回すスクリプトがサービスアカウントで `ci/gates` を出す。
11. **バックアップの道具と保管**。推奨: PostgreSQL は pgBackRest（IoT 12 §11.6 と同じ。WAL で RPO を縮められる）、リポジトリはファイルの複写。保管先と世代の数を教えてほしい。
12. **本番の Kanflow の DB の複写を使うリハーサル**（個人情報を含む）を社内の試験環境で行ってよいか、合成だけにするか。推奨: 合成で 2 回通してから、承認を得て複写で 1 回。
13. **その他**: 02 §8.1 の GitLab 互換の API をやめて GitStationSource だけにする（推奨: やめる）。切り替えまで本番の Kanflow を MySQL の修正だけの系統にする。Kanflow の PG 移行（K1）を GitStation の MR と並行して始めてよいか（推奨: 並行可。決定は「GitStation が先」なので、既定は GitStation の Phase 4 の後）。
14. **試験サーバーの試験の結果**（`test-server-guide.md` §6）。見つかった問題の一覧（P3 で直す）。

**答え（2026-11-02）**: すべて推奨どおり。Q2 は案 a（本番サーバーへビルドする時に今の Kanflow のデータを引き継ぐ）。Q1・Q6・Q7・Q11・Q12・Q14 の事実の情報は、該当する MR（P3・N1・O1・O2・K2）に着手する前に利用者に聞く。

## 11. P1 と R1 の下調べで決めたこと（2026-11-02）

ADR-0024 と、R1 の実装の地図（code-explorer）で決めた。計画の §3〜§5 より優先する。

**設計レビュー（2026-11-02）の決定は ADR-0024 末尾の表を正とする**（この節と食い違うときは表が優先する。M3-0 に P6 `fix/git-browse-reachability` を足した。§5.1）。

1. **R1 を 2 つに分ける**: R1a `feat/core-merge-requests`（表・権限・サービス・ルート・`gstn mr create/list/view/edit/close/reopen`）と R1b `feat/core-merge-request-versions`（push で版を足す、隠した ref、`merge_request.pushed`）。
2. **SQL は `forge-db/src/review/` に置く**（`validate-db-write-boundary` の規則 5・6）。`forge-review` はサービス層だけ（`forge-hosting` と同じ）。**`decided_callers.rs` の一律の例外に `forge-review` を足さない**（設計レビューで変更。ADR-0024 §11）。代わりに別のテスト `only_forge_review_calls_the_review_commands` を足し、forge-review の関数は `Authorized<MergeRequestCtx>`（マージは + `MergeClearance`）を型で取る。
3. **push から版へ**（R1b）: 購読者は git を呼べないので、`repository.pushed` を受けたら「MR を確かめ直すジョブ」を積むだけにする（購読者は forge-db の中なので `enqueue_on` を使える）。ジョブが実際のブランチを読み、版の行・隠した ref（`update-ref`）・`merge_request.pushed`（dedup key `merge_request_pushed:<mr_id>:<head_sha>`）を書く。push の記録が 100 本を超えて切れた（`refs_truncated`）ときは、そのリポジトリの開いている MR をすべて確かめ直す（黙って取りこぼさない）。版の行の base / merge-base は R2 のジョブで埋める。
4. **エラーコードの番号は登録の順**: R1a の DuplicateOpenMergeRequest は **FG-REVIEW-0001**。§4.4 の番号は仮。ADR-0024 が足した **MergeInProgress** も同じく登録の順。`forge-errors/build.rs` の AREAS に R1a で `REVIEW`、最初の INT のコードの MR で `INT` を足す。
5. **隠した ref** は版ごとの `refs/merge-requests/<iid>/v<n>` と `refs/merge-requests/<iid>/head`。`transfer.hideRefs = refs/merge-requests/` を `UPLOAD_PACK_CONFIG` と `RECEIVE_PACK_CONFIG` の両方に置く。want の確認は `exclude_glob` で `--exclude=refs/merge-requests/*` を手順 2 と 3 の両方の `--all` の前に置く（git 2.42 の `--exclude-hidden` は使わない。下限 2.38 のまま。glob と hideRefs は 1 つの定数から）。ADR-0007 の Amendment を R1b で書く。
6. **URL** は既存に合わせて `/api/v1/projects/{id}/merge-requests`（ハイフン）。
7. **MergeRequestCreate** はプロジェクトの Developer 以上（`RepoPush` と同じ段）。MR の作成は push ではないので、source のブランチの保護は見ない。
8. **プロジェクトの完全削除**（`projects/purge.rs`）で MR の表の行も消す（`forge_app` に DELETE の権限）。
9. **承認**（R5）は (head_sha, target_branch) に結ぶ（ターゲットを変えて承認を流用させない）。承認とマージには新しいトークンのスコープ `merge_approve` / `merge_execute` が要る（ADR-0017 §4.0、`api` では足りない。互いにも満たさない。スコープの CHECK を広げる追記のマイグレーションは変種を足す MR と同じ: R4 が `merge_execute`、R5 が `merge_approve`）。コミットした人の承認は数えない（既定で有効）、`.forge/**` と CODEOWNERS を変える MR は Maintainer の承認が要る（ADR-0017）。
10. **マージ**（R4）: tx1a（再判定）→ トランザクションの外で `merge-tree`・`commit-tree` → tx1b（再判定・`prepared` の行）→ フェンス（ロックの確認、`prepared → executing`）→ `update-ref` の CAS → tx2（`merged`・監査・`repository.pushed`・`merge_request.merged`、dedup は `merge_id`）。`prepared` / `executing` のまま残ったものは確かめ直すジョブが片づける（`executing` は git に聞く。ADR-0024 §4.2）。`update-ref` と `commit-tree` も `core.hooksPath` を空のディレクトリに固定する。
11. **作業項目のリンク**のマージ条件は、I1 が `work_items` を入れるまで設定できない（R6 では選べない形にする）。
12. **決定（R4、ADR-0024「実装で決めたこと（R4）」）**: マージの経路はサーバーのフックに頼らない。R4 はロックの下で、push の経路と同じ保護 ref の判定（`authorize()` と保護の規則）と、記録した head・target の先端を生きた値と照らし直す。push の経路の pre-receive / post-receive のフック（ADR-0008 が M3 に送ったもの）は M3 後半の X8 で扱う。
13. **M3-0 に MR を足す**（設計レビュー、ADR-0024 末尾の表の 25）: P6 `fix/git-browse-reachability`（R1 必須、R1a の前。評価役 `git-protocol-reviewer`・`security-reviewer`・`rust-reviewer`）。閲覧の API の SHA の指定に到達可能性の検査を足す。§5.1 の表に行を足した。
14. **実績から作るスケジュール（2026-11-02 追加）**: ロジックを含めて計画 12（`12-schedule-from-actuals.md`）に分けた。M3 と並行するのは Kanflow の E0（テンプレートの `key` と基準の保存）だけで、E1〜E5 は M3 後半〜M4、E6 は M5。GitStation の G1（開発状況 API の MR の時刻と規模）は I1 の後。
15. **L-5（衝突とブロックの解消案）を M5 へ前倒し（2026-11-02）**: 計画 01 §6 の L-5 を Must・M5 にし、止まっている理由（必須チェックの失敗・承認の不足・古いブランチ）の解消案まで広げた。適用はすべて人が判断する。
