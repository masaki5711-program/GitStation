# 20 M6 の計画 — ナレッジ（Wiki、個人ノート、横断検索）。Issue は取りやめ（2026-11-25）

作成: 2026-11-23（planner）。状態: **決定済み（2026-11-23）** — 利用者が 2026-11-23 に「計画 19・20・21 の未決の問いはすべて推奨の既定で」と決めた。§3 の推奨（I-1・W-C・N-B・S-b）と §10 の Q1〜Q8 の既定を採る（§10 の「決定」の列）。Q1・Q8 も事実を待たず既定で進める。

> **変更（2026-11-25、利用者の決定）**: GitStation に Issue は **作らない**。課題は Kanflow で管理する。Q1 は「作らない」（I-0）に変わり、I1〜I4 は **取りやめ**。Kanflow に結んだプロジェクトの「課題」は Kanflow へのリンクの 1 行だけ（§11 の文言のまま）。K・W・N の項目は変わらない。検索の `source_kind = issue` は作らない（K2 の値の一覧から外す）。
参照: 01 §3.5・§5（拡張点 1・5）・§8（M6）・§9、00 §1・§6、02 §4（作業項目プロバイダ）・F-12、08 §1、09 §1、13 §0・§3〜§5・§8.2、15（自分の作業のサイドバー）、18 S7（文書の速い経路）、ADR-0010・0015・0021・0023・0026・0027、CLAUDE.md §3。

## 0. 前提の点検

### 0.1 目標（1 文）

プロジェクトの説明・手順・決めごとを **計画 13 の文書の形のまま** Wiki と個人ノートに書け、プロジェクト・MR・Wiki・文書（と、作るなら Issue）を **1 つの検索窓から、読める物だけ** 日本語でも探せるようにする。新しい置き場所・新しい書式・外部の検索サービスは作らない。

### 0.2 決定済み（議論し直さない）

| 日付 | 決定 | この計画への効き方 |
|---|---|---|
| 2026-10-26 | 01 §3.5: **Kanflow との連携は forge 内の Issue より優先**。Issue は「Kanflow を使わないプロジェクト向け」 | Issue は最小にし、最後に回す（§3.1） |
| 2026-10-31 | 00 §6・08 §1: **入口は Kanflow 1 つ**。PBI・スプリント・教訓（W11）は Kanflow が持つ | Issue に計画・スプリント・見積を持たせない。教訓の検索は Kanflow のもの |
| 2026-11-01 | ADR-0023: **文書ポータルをやめ、画面の系統はリポジトリの画面 1 つ**。リポジトリの種類もやめた | Wiki に別の画面の系統・別の種類のリポジトリを作らない |
| 2026-11-09 | 計画 13・ADR-0026: 文書は `.md` + frontmatter（`doctype`・`id`・`title`・`owner`）、`[[ID]]` の参照、文書の API / JSON、`forge-doc` は純粋 | Wiki のページとノートは計画 13 の文書。描画・lint・参照・API を作り直さない |
| 2026-11-12 | E1 の簡単な編集（1 ファイルのコミット、直接または MR） | Wiki とノートの保存はこの経路 |
| 2026-11-17 | 計画 18 S7: 文書だけの MR の速い経路 | Wiki を MR で回すと決めた場合でも数秒で着地する |

### 0.3 コードで確かめたこと（この worktree の `main`）

- **1 プロジェクト = 1 リポジトリ**: `forge.repositories` は `UNIQUE (project_id)`（`crates/forge-db/migrations/20261031100000_projects_and_repositories.sql`）。保存先はリポジトリの id から（ADR-0009）。GitLab 型の `{path}.wiki.git`（2 つめのリポジトリ）は、この前提を崩す。
- ADR-0015 は `/-/issues`・`/-/wiki`・`{path}.wiki.git` を **M6 用に予約だけ** している。名前の末尾 `.wiki` は禁止（`crates/forge-core/src/namespace_path.rs:53`）。
- **文書の API は毎回 git から読む**（`crates/forge-web/src/routes/documents.rs`。索引は `forge_checks::doc_lint::read_tree` の上限つきの読み）。文書の DB の索引は無い。
- **全文検索は無い**: マイグレーションにもコードにも `tsvector`・`pg_trgm`・`to_tsquery` が無い。
- 一覧は **行ごとに `authorize()` で決め、SQL では決めない**（`crates/forge-web/src/routes/lists.rs` の案 L1、走査の上限、封印したカーソル、同時実行の上限 `FG-WEB-0008`）。検索の結果も同じ形にする。
- 簡単な編集のコミット `POST …/repository/commits/single-file`（`crates/forge-web/src/routes/file_commit.rs`）は、push と同じ判定・秘密の検査・CAS で、直接または新しいブランチ + MR。
- 連携の種類は `kanflow` だけ（`20261114100000_integrations.sql:32`）。`work_items.ref` は `^PBI-…$` だけで、連携に結び付く。**forge 内の Issue はこの表に入らない**（入れるなら別の表）。
- エラーの領域（`crates/forge-errors/build.rs:41`）に知識・検索の領域は無い。利用者の名前空間（`namespaces.kind = 'user'`）はある。

### 0.4 前提への意見（何がまだ要るか）

| 01 の項目 | Kanflow と計画 13 の後でも要るか | 結論 |
|---|---|---|
| 軽量な Issue | Kanflow に結んだプロジェクトでは **要らない**（二重の課題管理になる）。Kanflow を使わないプロジェクト（社外との小さな共同作業、PFS の試験のリポジトリなど）があるかは事実が要る | ~~最小の形（§3.1）で最後に~~ → **作らない**（2026-11-25、Kanflow で管理）。Q1 |
| Wiki | 要る。計画 13 の文書は「統制された文書」（版・承認・型）で、手順メモ・はじめての人向けの説明・会議の決めごとのような **軽い説明の置き場** が無い。ただし書式と描画は計画 13 を使う | 作る（§3.2） |
| 個人ノート | 要る（自分の作業のメモ、下書き）。ただし新しい保存の仕組みは要らない | 普通の非公開プロジェクトの便利な入口として作る（§3.3） |
| 横断検索 | 要る。今はプロジェクトの名前の一覧と、要件 ID の検索しかない。文書の数が増えると探せない | 作る。**最初に**（他の 3 つも検索に載るため） |

## 1. M6 の範囲

| 項目 | 区分 | 出典 |
|---|---|---|
| エージェントモデル（領域 `KNOW`、評価役の観点と eval） | 必須・最初 | CLAUDE.md §3-1、00 §8 |
| 横断検索（プロジェクト・MR・文書・Wiki・ノート。日本語） | 必須 | 01 §3.5 |
| Wiki（プロジェクトごと、内部は Git、計画 13 の文書） | 必須 | 01 §3.5 |
| 個人ノート | 必須 | 01 §3.5 |
| ~~軽量な Issue~~ | **取りやめ**（2026-11-25、Q1 = 作らない。課題は Kanflow） | 01 §3.5 |
| AI の文脈 API・MCP からの検索 | M6 の後半（任意） | 01 §3.6、02 §6 |
| コード検索 | M8 のまま（`git grep`） | 01 §3.5 |

## 2. 依存の順

```
K0[root] ─ ADR ─ K1 tokenizer ─ K2 index(DB sources) ─ K3 search API ─┬─ K5 UI
                                                                    ├─ K4 文書の索引 ─┐
W1 wiki の結び付けと役割 ─ W2 Wiki を作る ─┬─ W3 [[wiki:…]] ──────────────────────┤
                                       ├─ W4 Wiki の画面 ─────────────────────┤
                                       └─ N1 ノート ─ N2 ノートの画面 ───────────┘
（取りやめ 2026-11-25: I1 ─ I2 ─ I3 ─ I4。課題は Kanflow で管理）
```

W1 は K1〜K3 と並行できる（触るファイルが別）。検索に Wiki とノートが載るのは K4 の後（どちらも普通のプロジェクトの文書なので、K4 がそのまま索引する）。

## 3. 設計の選択肢と推奨

### 3.1 「軽量な Issue」とは何か

| 案 | 中身 | 長所 | 短所 |
|---|---|---|---|
| **I-0 作らない（採用、2026-11-25）** | Kanflow に結んだプロジェクトの「課題」の欄は Kanflow へのリンクだけ | 二重の管理がない。保守が増えない | Kanflow を使わないプロジェクトに課題の置き場が無い |
| I-1 最小の Issue（2026-11-23 の推奨。2026-11-25 に取りやめ） | 題名・本文（計画 13 の描画、`[[…]]` が使える）・開閉・ラベル・担当・コメント・`#n` の参照、`Closes #n` の MR のマージで閉じる。**マイルストーン・見積・スプリント・ボードは作らない**（Kanflow の領分）。プロジェクトの設定 `issues.enabled`（既定 `false`）。**Kanflow に結んだプロジェクトでは有効にできない**。後から Kanflow に結ぶときは、開いている Issue を Kanflow の Icebox に送り（02 F-12 の `POST /icebox`）、Issue は読み取りだけにする | 「Kanflow を使わない」場合の受け皿で、Kanflow へ移る道がある | 表と画面が 1 組増える |
| I-2 GitLab 並みの Issue | マイルストーン・ボード・重み・期限 | 移行元と同じ | Kanflow と真っ向から重なる。08 §1 の「入口は 1 つ」に反する |

**決定（2026-11-25）: I-0。** 以下は 2026-11-23 の推奨の記録（I-1 を最後に作る）。 01 のラベル・マイルストーン・コミットからの参照のうち、マイルストーンは Kanflow のスプリントと重なるので落とす。`work_items` には入れない（`PBI-` だけで連携に結び付く表なので）。マージの条件「作業項目のリンク」は、Issue が有効なプロジェクトでは `#n` も数える（I2 で 1 行の拡張）。

### 3.2 Wiki の置き場所

| 案 | 中身 | 長所 | 短所 |
|---|---|---|---|
| W-A 専用の 2 つめのリポジトリ（`{path}.wiki.git`、GitLab 型） | プロジェクトに Wiki のリポジトリを足す | GitLab と同じ見た目 | `repositories` の `UNIQUE (project_id)` と、リポジトリ id を前提にした保存先・バックアップ・ミラー・権限の Resource・clone の URL・doctor をすべて直す。ADR-0023 で捨てた「種類」を別の名前で戻すことになる |
| W-B 同じリポジトリの `wiki/` フォルダ | 主リポジトリに Wiki のページを置く | 何も足さない | Wiki の 1 行の直しにも、コードの保護ブランチ（承認・必須チェック・CI）が掛かる。外すにはパスごとの承認の免除という **新しいマージの規則** が要る。非エンジニアに Wiki だけ書かせられない。コードの履歴が Wiki の直しで埋まる |
| **W-C Wiki = 結び付けた普通のプロジェクト（推奨）** | 「Wiki を作る」で同じ名前空間に `<project>-wiki` という **普通のプロジェクト** を作り、`project_wikis` で結ぶ。ページは計画 13 の文書（doctype `wiki.page`）。主プロジェクトのメンバーは Wiki に **同じ役割を継ぐ**（片方向、推移しない）。Wiki だけのメンバー（非エンジニア）も足せる | 保存・clone・バックアップ・ミラー・文書の API・lint・描画・簡単な編集・検索を **そのまま使う**。保護の設定が別なので「Wiki は直接保存、コードは MR」が設定だけで済む。履歴が分かれる | プロジェクトが 1 つ増えて見える（一覧では Wiki の印を付けて主プロジェクトの下に出す）。役割の継承という認可の変更が 1 つ要る |

**推奨: W-C。** 新しい仕組みは「結び付けの表」と「役割の継承」の 2 つだけで、どちらも `forge-auth` の 1 か所で決まる（CLAUDE.md §3-4）。`{path}.wiki.git` と `/-/wiki` は予約のまま、`/-/wiki` は結び付けた Wiki の画面への入口に使う（ADR-0015 の予約の解除は ADR で）。

Wiki のページの形（`.forge/doctypes/wiki.page.yml`、Wiki を作るときに置く）:

- 必須の 4 項目は計画 13 のまま。**利用者は YAML を書かない**（簡単な編集のフォームが作る）。`id` は `WIKI-0001` からの連番（Q6）、`owner` は既定で名前空間のグループ、`classification` は既定 `internal`。
- 見出し・ブロックの決まりは無し（軽い説明の置き場なので）。lint は計画 13 の決定的な検査だけ（壊れた参照・ID の重複・HTML の直書き）。
- 保存は **既定で main への直接のコミット**（Wiki のプロジェクトの保護の既定で Developer の直接の push を許す。Q3）。`forge/doc-lint` は push の後のチェックとして走り、赤なら Wiki の画面に「直す」の 1 行を出す。版の比較と戻しは Git の履歴。
- 主リポジトリの文書から Wiki へは **`[[wiki:WIKI-0012]]`**（版を固定しない。Wiki の既定ブランチの先端で解決）。壊れた `wiki:` の参照は lint の **警告**（Wiki は別の速さで変わるので、主リポジトリの MR を止めない）。読めない人には ID だけ（計画 13 Q14 と同じ）。

### 3.3 個人ノート

| 案 | 中身 | 判断 |
|---|---|---|
| N-A DB の表に本文 | `notes(user_id, body)` | 版も clone も無く、検索・描画・秘密の検査を別に作る。採らない |
| **N-B 自分の名前空間の非公開プロジェクト `<user>/notes`（推奨）** | 「ノートを作る」で 1 つ作り、`user_notes` で指す。ページは doctype `note`（`id` は `N-YYYYMMDD-HHMMSS` を自動、`owner` は本人 — doctype の `owner_kind: user` を `forge-doc` に足す）。すばやいメモは main への直接のコミット | 普通のプロジェクトなので、clone・履歴・検索・秘密の検査がそのまま。共有したくなったらメンバーを足すだけ |

### 3.4 検索の方式（PostgreSQL 16、外部の検索サービスなし）

| 案 | 中身 | 長所 | 短所 |
|---|---|---|---|
| S-a `pg_trgm`（contrib） | 本文に trigram の GIN、`ILIKE` / 類似度 | 拡張は PG に同梱 | 日本語の扱いが **DB のロケール（LC_CTYPE）に依存** し、開発（コンテナ）と PFS の本番で同じと言い切れない。2 文字の語（「設備」「承認」）が 3-gram に足りない。順位付けが弱い |
| **S-b アプリ側の正規化 + 2-gram を `tsvector` に（推奨）** | 純粋な Rust の関数（`forge-core::search_text`）で NFKC・小文字化 → 和文（漢字・かな・カナ）の連続は 2-gram、ラテン文字・数字の連続は語（en / id）に分け、`array_to_tsvector` で保存。重みは題名 A・見出しと ID B・本文 C。問い合わせも同じ関数で作り、`@@` + `ts_rank_cd` + 新しさで並べる。最後の語は前方一致（`:*`） | ロケールに依存しない。拡張が要らない。分かち書きの規則が **Rust の単体テストと性質のテスト** で固定でき、Claude が結果の理由を説明できる | 2-gram は偽陽性がある（「京都」で「東京都」が当たる）。順位で下げ、必要なら本文で位置の確かめをする |
| S-c PGroonga / pg_bigm | 日本語向けの拡張 | 精度が良い | PG の配布物の外。TimescaleDB の像・PFS のサーバーに拡張を足す運用が増え、開発と本番の差の原因になる。採らない |
| S-d Elasticsearch / Meilisearch | 別のサービス | 機能が多い | 運用するサービスが増える。条件（外部の検索サービスなし）に反する。採らない |

**推奨: S-b。** 索引は派生のデータ（いつでも作り直せる）なので、追記だけの表ではなく、書き換え・削除のできる表にする（`tables.toml` の台帳で「派生」と記す）。

**何を索引するか**

| 種類 | 中身 | 更新のきっかけ |
|---|---|---|
| プロジェクト | パス・名前・説明 | プロジェクトのイベント（作成・名前の変更・削除） |
| MR | 題名・説明（差分は入れない） | MR のイベント |
| 文書（計画 13） | 既定ブランチの先端の文書: 題名・ID・見出し・ブロックの ID と本文。Wiki とノートもこれ | `repository.pushed`（既定ブランチだけ、変わったパスだけ。最初は全体） |
| ~~Issue（I-1 のとき）~~ | 取りやめ（2026-11-25、Issue は作らない） | — |

- 既定ブランチ以外は索引しない（Q5）。コードは索引しない（M8）。
- 1 件の本文は 256 KB まで。超えた分は索引せず `truncated` の印を付け、ログに理由を残す（黙って切らない）。読みの上限に当たったリポジトリは文書の API と同じく `incomplete`。
- 機密の区分は **既定ブランチの先端での区分** を行に持つ（分からなければ `confidential`、閉じる側）。

**読める物だけを返す（CLAUDE.md §3-4）**

- `GET /api/v1/search` は `SearchQuery`（インスタンス、サインインした利用者、トークンは `read_api`）を registry で判定する。
- 候補は順位の順に読み、**1 件ずつ `authorize()`**（プロジェクトごとに 1 回の判定をリクエストの中で使い回す）: プロジェクトは `ProjectRead`、文書・Wiki・ノートは `RepoBrowse`、MR は MR の読みの Action、Issue は `IssueRead`。SQL の条件は「読める見込みのある範囲に絞る」ためだけに使い、広げる方向には決して使わない。
- 走査の上限（一覧と同じ形）。足りないときは短いページで `has_more: true`。**件数は出さない**（読めない物の数が分かってしまう）。カーソルは利用者ごとに封印。
- AI の利用者（`audience`、計画 06 D-5）には `confidential` の行を返さず、数えもしない（文書の API と同じ規則）。
- 抜粋（スニペット）は保存した本文から作り、強調は **文字の範囲** で返す（HTML を返さない。画面は `<mark>` を自分で組む）。

**性能の目標**: 10 万件で p95 ≤ 300 ms（01 §9 の一覧と同じ）、文の時間切れ 2 s。push から索引まで p95 ≤ 60 s。

### 3.5 クレートの置き場所（CLAUDE.md §3-9）

- 分かち書き（純粋）: `forge-core::search_text`。
- 索引の表と問い合わせ: `forge-db`。
- 新しい機能クレート `forge-search`: 問い合わせの組み立て・順位・抜粋・結果の判定の手順（`forge-auth` と `forge-db` に依存。他の機能クレートには依存しない）。
- 文書の索引の worker は `forge-cli`（`forge_checks::doc_lint::read_tree` で読み、`forge_search` で書く — 機能クレート同士をつながない）。
- Wiki・ノートの作成は、プロジェクトの作成の既存の場所（`forge-hosting`）。~~Issue は I-1 のとき新しい機能クレート `forge-issues`~~（取りやめ 2026-11-25）。

## 4. 設計の要点

### 4.1 データ（すべて追記のマイグレーション。`tables.toml` の台帳と doctor の期待値も更新）

| MR | 表 |
|---|---|
| K2 | `search_entries`（`source_kind` = project / merge_request / document（`issue` は取りやめ 2026-11-25）、`project_id`、`source_key`、`title`、`body_text`、`tokens tsvector`、`classification`、`truncated`、`source_updated_at`、`indexed_at`。`UNIQUE (source_kind, project_id, source_key)`、`tokens` に GIN）、`search_index_state`（プロジェクトごと: 索引した SHA、状態、最後のエラーコード、試行の数） |
| W1 | `project_wikis`（`project_id` PK、`wiki_project_id` UNIQUE、自分自身・連鎖を禁止する CHECK とトリガ、`linked_by`・`linked_at`、監査の行あり） |
| N1 | `user_notes`（`user_id` PK、`project_id` UNIQUE） |
| ~~I1~~（取りやめ 2026-11-25） | ~~`issues`（`project_id`・`iid`・題名・本文・状態・作成者・担当・`closed_by_merge_request_id`・版）、`issue_counters`、`issue_labels`、`issue_label_links`、`issue_notes`、`merge_settings` か専用の表に `issues_enabled`~~ |

### 4.2 Action と権限の表（どれも allow と deny の行。`authz-matrix-auditor` が確かめる）

| Action | allow | deny（代表） |
|---|---|---|
| SearchQuery | サインインした有効な利用者、`read_api` のトークン、エージェント（機密は除いて） | 匿名、無効な利用者、Runner・Job、ポータルの資格情報 |
| （結果の 1 件） | 既存の `ProjectRead` / `RepoBrowse` / MR の読み（`IssueRead` は取りやめ） | 読めないプロジェクトの行は題名もパスも出さない（行の deny のテスト） |
| ProjectWikiLink（作成と結び付け・外す） | 主プロジェクトの Maintainer 以上かつ名前空間で `ProjectCreate` できる **人**（Web のセッションまたは `api` のトークン） | Developer、エージェント、サービスアカウント、代理、Job |
| （役割の継承） | 主プロジェクトの役割を Wiki に同じ強さで継ぐ。Wiki 自身の所属と大きい方 | Wiki のメンバーは主プロジェクトに何も継がない。継承は 1 段だけ（Wiki の Wiki は無い）。主プロジェクトから外れたら次のリクエストから Wiki も失う |
| PersonalNotesCreate | 本人だけ（自分の名前空間）、人 | 他人の名前空間、サービスアカウント、代理、Job |
| ~~IssueRead / IssueCreate / IssueComment~~ | 取りやめ（2026-11-25、Issue は作らない） | — |
| ~~IssueUpdate / IssueClose~~ | 取りやめ（2026-11-25、Issue は作らない） | — |
| ~~IssueLabelManage~~ | 取りやめ（2026-11-25、Issue は作らない） | — |
| ~~IssuesSettingManage~~ | 取りやめ（2026-11-25、Issue は作らない） | — |

ノートの読み書き・Wiki の読み書きは **既存の Action**（`RepoBrowse`、`RepoPush`、簡単な編集のコミット）で、新しい Action は要らない。

### 4.3 エラーコード（仮。領域 `KNOW` を新設、番号は登録の順）

| 仮の名前 | 内容 | 重さ | runbook |
|---|---|---|---|
| KNOW-a SearchUnavailable | 検索の文が時間切れ・DB の失敗。利用者には「いま検索できません」+ `request_id` | warn | `search-index` |
| KNOW-b SearchIndexLagging | 索引の遅れ（最古の未処理の push が 10 分を超える）。doctor と alert | **alert** | `search-index` |
| KNOW-c SearchIndexItemFailed | 1 件・1 リポジトリの索引の失敗（読みの上限、解析の失敗、`truncated`）。状態の表に残し、次の push・再索引でやり直す | warn | `search-index` |
| KNOW-d WikiLinkRefused | 結び付けの拒否（別の名前空間、既に結んである、連鎖、名前の衝突） | info | `knowledge` |
| KNOW-e WikiProvisionFailed | Wiki のプロジェクトの作成・初期のコミットの失敗（結び付けは作らない。途中のプロジェクトは既存の作成の失敗の後始末） | warn | `knowledge` |
| KNOW-f NotesProjectMissing | ノートの指す先が削除・移動された（作り直しの案内） | info | `knowledge` |
| ~~KNOW-g IssuesDisabled~~ | 取りやめ（2026-11-25、Issue は作らない） | — | — |
| ~~KNOW-h IssueVersionConflict~~ | 取りやめ（2026-11-25、Issue は作らない） | — | — |
| DOC-next WikiReferenceBroken | `[[wiki:…]]` が解決できない（lint の警告） | info | — |

問い合わせの形の誤り（空、2 文字未満、長すぎ、不明な `scope`）は既存の `FG-WEB-0006 reason=param_invalid`、同時実行の上限は `FG-WEB-0008`、読めない物は `FG-REPO-0005`（存在を明かさない）。

## 5. MR の分け方

置き場所: **[root]** = エージェントモデル（先に入れる）、印なし = GitStation。各 MR の終わりは計画 14 §5 と同じ（fmt・clippy `-D warnings`・nextest、`gen-catalog` / `gen-authz` の差分なし、`/review` で CRITICAL / HIGH なし、評価役は 2 つまで）。`gstn` のコマンドはルートと同じ MR。

| MR | ブランチ | 中身 | 先に書くテスト（例） | 評価役（≤2） | 前提 |
|---|---|---|---|---|---|
| **K0 [root]** | `chore/harness-m6-knowledge-area` | 領域 `KNOW` を `forge-error-contract.md`・`validate-error-registry.js` に。`security-reviewer` に「検索の結果・件数・抜粋から読めない物が漏れないか」、`database-reviewer` に「派生の索引の表、`tsvector` の大きさ、文の時間切れ」の観点。changelog | eval `search-leaks-invisible-title`、`search-count-oracle` | `harness-optimizer` | 今すぐ |
| **ADR** | `docs/docs-adr-knowledge-wiki-search` | ADR（新規、番号は着手時の次）: Wiki = 結び付けた普通のプロジェクトと役割の継承、ノート = 自分の非公開プロジェクト、検索 = アプリ側の 2-gram + `tsvector`、ADR-0015 の `/-/wiki` の予約の解除。Issue は作らない（Q1、2026-11-25） | — | `security-reviewer` | K0 |
| **K1** | `feat/core-search-tokenizer` | `forge-core::search_text`（NFKC、小文字、和文 2-gram、ラテン語の語、問い合わせの組み立て）、`forge-errors` の AREAS に `KNOW` | `query_tokens_subset_of_text_tokens_when_substring`（性質）、`fullwidth_and_halfwidth_match`、`two_char_japanese_word_found`、`indonesian_words_split_on_spaces`、`tokenize_is_deterministic` | `rust-reviewer` | ADR |
| **K2** | `feat/db-search-index` | `search_entries`・`search_index_state`、プロジェクトと MR のイベントの購読者（outbox）、作り直し `forge search reindex [--project]`、doctor `search.index_lag` | `mr_title_edit_reindexed`、`deleted_project_rows_removed`、`subscriber_failure_is_retried_not_dropped`、`statement_timeout_is_know_a`、`reindex_is_idempotent` | `database-reviewer`、`silent-failure-hunter` | K1 |
| **K3** | `feat/web-search-api` | `GET /api/v1/search?q=&scope=&project=&cursor=`、1 件ずつ `authorize()`、走査の上限・封印したカーソル・同時実行の上限、AI の利用者の機密の除外、抜粋の文字の範囲、`gstn search`、権限の表 | `invisible_project_title_never_returned`、`guest_on_private_project_gets_no_hits`、`no_total_count_in_response`、`agent_never_sees_confidential_hit`、`cursor_of_other_user_rejected`、`denied_for_runner_and_job` | `security-reviewer`、`authz-matrix-auditor` | K2 |
| **K4** | `feat/core-search-document-indexer` | `repository.pushed`（既定ブランチ）で文書を索引（変わったパスだけ、最初は全体）。`read_tree` の上限、`truncated`、先端の機密の区分。worker は `forge-cli` | `push_to_default_branch_reindexes_changed_docs_only`、`other_branch_push_ignored`、`confidential_at_tip_marked`、`bound_hit_logged_as_know_c`、`deleted_doc_removed`、`block_id_exact_query_hits` | `silent-failure-hunter`、`database-reviewer` | K3 |
| **K5** | `feat/ui-search` | 上部の検索窓（`/` で移る）、結果の画面（種類ごとの絞り込み、プロジェクトのパス、抜粋）、空・失敗・読み込み中、ja / en / id | E2E: `search_japanese_title`、`no_results_next_step`、axe と CSP の違反なし | `typescript-reviewer`、`a11y-architect` | K3 |
| **W1** | `feat/auth-wiki-link-role` | `project_wikis`、役割の継承（`forge-auth` の役割の解決の 1 か所）、ProjectWikiLink の行 | `owner_project_developer_can_push_wiki`、`wiki_only_member_gets_nothing_on_owner`、`inheritance_not_transitive`、`removed_member_loses_wiki_next_request`、`link_denied_for_agent_service_account_act_as` | `security-reviewer`、`authz-matrix-auditor` | ADR（K1〜K3 と並行） |
| **W2** | `feat/web-wiki-create` | `POST /api/v1/projects/{id}/wiki`（作成 + 初期のコミット: `.forge/doctypes/wiki.page.yml`・`WIKI-0001.md`・保護の既定 + 結び付けを 1 つの手順で。失敗したら結ばない）、外す、`gstn wiki create` | `seed_passes_doc_lint`、`provision_failure_leaves_no_link`、`name_collision_is_know_d`、`unlink_keeps_wiki_project`、`owner_project_deleted_keeps_wiki` | `git-protocol-reviewer`、`silent-failure-hunter` | W1 |
| **W3** | `feat/core-doc-wiki-references` | `forge-doc` に `[[wiki:<id>]]` / `[[wiki:<id>#<block>]]`、描画で結び付けた Wiki の先端に解決、読めなければ ID だけ、壊れたら lint の警告 | `wiki_ref_parses`、`unreadable_wiki_ref_shows_id_only`、`broken_wiki_ref_is_warning_not_error`、`no_wiki_linked_is_warning` | `rust-reviewer`、`security-reviewer` | W2 |
| **W4** | `feat/ui-wiki` | プロジェクトの「Wiki」タブ（`/-/wiki`）: ページの一覧（フォルダの木）、表示（文書の API）、作成と編集（簡単な編集 + frontmatter のフォーム、YAML を見せない）、履歴、「Wiki を作る」の空の状態、lint が赤のときの 1 行 | E2E: `create_wiki_then_first_page`、`non_engineer_edits_without_yaml`、`lint_red_shows_next_step` | `design-reviewer`、`a11y-architect` | W2 |
| **N1** | `feat/web-personal-notes` | `user_notes`、`POST /api/v1/user/notes`（`<user>/notes` を非公開で作成、doctype `note`、`owner_kind: user` を `forge-doc` に）、すばやいメモ（直接のコミット）、`gstn note new` | `other_user_cannot_read_notes`、`notes_create_denied_for_service_account_and_act_as`、`notes_not_in_others_search`、`owner_kind_user_lint`、`moved_notes_project_is_know_f` | `security-reviewer`、`authz-matrix-auditor` | W2 |
| **N2** | `feat/ui-notes` | 自分の作業のサイドバー（計画 15）に「ノート」、すばやいメモの入力、一覧と検索 | E2E: `quick_note_saved_and_found` | `design-reviewer`、`i18n-sync-auditor` | N1、K4 |
| **R** | `docs/ops-knowledge-runbooks` | runbook `search-index`・`knowledge`（§6）、crate の README | — | `error-contract-auditor` | K2 と同時に始め、W2 で足す |
| ~~I1~~ **取りやめ**（2026-11-25、課題は Kanflow） | `feat/db-issues` | §4.1 の表、`issues.enabled`（Kanflow に結んだら有効にできない） | `enable_refused_when_kanflow_bound`、`iid_per_project_gapless_under_concurrency` | `database-reviewer`、`authz-matrix-auditor` | **Q1 が「作る」**、K3 |
| ~~I2~~ **取りやめ**（2026-11-25、課題は Kanflow） | `feat/web-issues` | API・`#n` の参照（コミット・MR）、`Closes #n` のマージで閉じる、イベント（Webhook・検索の索引）、マージの条件で `#n` を作業項目のリンクに数える、`gstn issue` | `closes_on_merge_to_default_only`、`ref_to_other_project_ignored`、`version_conflict_is_know_h`、`disabled_project_answers_not_found` | `security-reviewer`、`rust-reviewer` | I1 |
| ~~I3~~ **取りやめ**（2026-11-25、課題は Kanflow） | `feat/ui-issues` | 「課題」タブ。Kanflow に結んだプロジェクトでは「課題は Kanflow で管理しています — 開く」の 1 行だけ | E2E: `kanflow_bound_shows_link_only` | `design-reviewer`、`i18n-sync-auditor` | I2 |
| ~~I4~~ **取りやめ**（2026-11-25、課題は Kanflow） | `feat/cross-issues-to-kanflow` | Kanflow に結ぶときに開いている Issue を Icebox へ送る（冪等キー）、送った後は読み取りだけ | `export_is_idempotent`、`kanflow_down_keeps_issues_writable_and_logs` | `silent-failure-hunter`、`security-reviewer` | I2、02 F-12 |
| X1（任意） | `feat/web-context-search` | AI の文脈 API・MCP に検索の道具（AI の利用者の規則のまま） | `agent_search_withholds_confidential` | `security-reviewer` | K4 |

並行の目安: 1 本目の worktree で K1→K2→K3→K4、2 本目で W1→W2→W3、UI（K5・W4・N2）は API の後に 3 本目。Rust のビルドは同時に 2 つまで。

## 6. 運用で足すもの

- **`forge doctor`**: `search.index_lag`（最古の未処理の push、件数）、`search.index_state`（失敗の続くプロジェクト）、`knowledge.wiki_links`（指す先の無い結び付け）。
- **メトリクス**: 検索の時間（p50 / p95）、時間切れの数、索引の遅れ、索引の失敗、行の数と表の大きさ。
- **ログ**: すべての失敗に `request_id`・`error_code`。検索の問い合わせの文字列は **ログに出さない**（個人の調べ物・機密の語が残るため。長さと種類だけ）。索引は `project_id`・`sha`・`source_key`。
- **runbook**: `search-index`（遅れの見方、`forge search reindex` の作り直し、時間切れ、`truncated` の扱い、分かち書きの規則を変えたら全体の作り直し）、`knowledge`（Wiki の結び付けと外し、主プロジェクトの削除の後、ノートの作り直し、役割の継承の確かめ方）。
- **バックアップ**: 索引は派生なので復元の後に `forge search reindex` で作る（`backup-restore` に 1 行）。Wiki・ノートは普通のプロジェクトなので既存のバックアップとミラーに入る。

## 7. 危険と対策

| 危険 | 対策 |
|---|---|
| 検索の結果・件数・抜粋・順位から、読めない物の存在が分かる | 1 件ずつ `authorize()`、件数を出さない、カーソルの封印、K0 の eval 2 つ |
| AI に機密の文書が渡る | 先端の区分を行に持ち、分からなければ `confidential`。AI の利用者には返さず数えない |
| 役割の継承が広がりすぎる | 片方向・1 段だけ・`forge-auth` の 1 か所。外したら次のリクエストから効く。行の deny のテスト |
| 2-gram の偽陽性で探しにくい | 題名・ID の重み、新しさ、利用者の評価で目立てば本文の位置の確かめを足す（計測してから） |
| 索引が黙って古くなる | 状態の表・KNOW-b の alert・doctor。失敗した行は残し、次でやり直す |
| Issue が Kanflow と二重になる | Issue を作らない（2026-11-25、Q1）。課題は Kanflow だけ |
| Wiki の直接の保存で壊れた参照が入る | push の後の `forge/doc-lint` と画面の 1 行。参照は ID なのでファイルの移動では切れない |

## 8. 完了の条件

1. 日本語の 2 文字の語・全角半角の違い・文書のブロックの ID で、プロジェクト・MR・文書・Wiki・ノートが探せる。読めない物は 1 件も出ず、件数でも分からない（K3 のテストと K0 の eval が緑）。
2. あるプロジェクトで「Wiki を作る」から、非エンジニアが YAML を見ずにページを作り、主リポジトリの文書から `[[wiki:…]]` で参照できる。主プロジェクトから外れたメンバーは次のリクエストから Wiki を読めない。
3. 個人ノートを作り、すばやいメモが 60 秒以内に検索に出て、他人には出ない。
4. 新しいルートと Action はすべて権限の表にあり、カタログ・runbook・ADR がそろう。10 万件で p95 ≤ 300 ms を測って記録。
5. ~~（Q1 が「作る」のとき）Issue を `Closes #n` で閉じられ、結ぶと Icebox へ移る~~ — 取りやめ（2026-11-25、Q1 = 作らない）。

**確かめのコマンド**: 計画 14 §8 と同じ（fmt・clippy `-D warnings`・nextest・`gen-catalog` / `gen-authz` の差分なし・`node scripts/self-heal.js --check`）。

## 9. UI の要点（ja / en / id、08・計画 15、「見やすく・使いやすく・かっこよく」）

| 画面 | ja | en | id | 要点 |
|---|---|---|---|---|
| 上部の検索窓 | プロジェクト・MR・Wiki・文書を検索 | Search projects, MRs, wiki and documents | Cari proyek, MR, wiki, dan dokumen | どの画面にも 1 つ。`/` で移る。結果は種類ごとに分け、プロジェクトのパスと抜粋（`<mark>` は画面で組む） |
| 結果なし | 見つかりませんでした。言葉を短くするか、種類の絞り込みを外してください | No results. Try a shorter word or remove the filter | Tidak ada hasil. Coba kata yang lebih pendek atau hapus filter | 「次に押すもの」を 1 行で |
| 検索の失敗 | いま検索できません（問い合わせ番号: …） | Search is unavailable (request ID: …) | Pencarian tidak tersedia (ID permintaan: …) | `user_message` + `request_id` だけ |
| Wiki のタブ（空） | Wiki はまだありません — 「Wiki を作る」 | No wiki yet — "Create wiki" | Belum ada wiki — "Buat wiki" | Maintainer にだけボタン。他の人には「管理者に頼んでください」 |
| Wiki の lint が赤 | このページに直す所があります（壊れた参照 2 件）— 開いて直す | This page needs fixes (2 broken links) — Open and fix | Halaman ini perlu diperbaiki (2 tautan rusak) — Buka dan perbaiki | 色だけに頼らない |
| ノート | 自分のノート / すばやいメモ | My notes / Quick note | Catatan saya / Catatan cepat | 自分の作業のサイドバー。保存は 1 クリック |
| 課題（Kanflow に結んだ） | 課題は Kanflow で管理しています — 開く | Issues are managed in Kanflow — Open | Isu dikelola di Kanflow — Buka | タブの中身はこの 1 行だけ |

- 編集は E1 の簡単な編集 + frontmatter のフォーム（型の定義から作る）。YAML・ブランチ・コミットの語は既定で隠し、「詳しく」で開く。
- インドネシア語は長くなるので、ボタンとタブは折り返しと省略の両方を E2E で確かめる（`i18n-sync-auditor`）。
- 色・余白・字はトークン（`tokens.css`）だけ。結果の種類はアイコン + 文字（色だけで区別しない）。

## 10. 利用者に聞くこと（推奨の既定つき）

| # | 問い | 推奨の既定 | 決定 |
|---|---|---|---|
| Q1 | **Kanflow を使わないプロジェクトはあるか**（あるなら Issue を作る） | I-1 を作るが **既定は無効で最後に**。該当が無ければ I-0（作らず、Kanflow へのリンクだけ） | **決定: 作らない（I-0）。課題は Kanflow で管理する（2026-11-25、利用者の決定）。** 2026-11-23 の「I-1 を既定は無効で最後に」を置き換える |
| Q2 | Wiki のメンバーは主プロジェクトの役割を継ぐか | **継ぐ**（片方向・1 段）。Wiki だけのメンバーも足せる | 決定: 継ぐ（片方向・1 段）。Wiki だけのメンバーも足せる（2026-11-23） |
| Q3 | Wiki の保存は直接のコミットか、MR か | **直接のコミット**（Developer 以上）。lint は保存の後。統制が要る文書は Wiki ではなく主リポジトリの文書（承認のフェーズ付き）に置く | 決定: 直接のコミット（Developer 以上）、lint は保存の後（2026-11-23） |
| Q4 | ノートの既定の機密の区分と、管理者が読めるか | 区分は **`internal`**（本人が `confidential` に変えられる）。管理者は既存の規則のまま（読むと監査に残る） | 決定: `internal`（本人が `confidential` に変えられる）、管理者は既存の規則のまま（2026-11-23） |
| Q5 | 検索は既定ブランチだけか | **既定ブランチの先端だけ** | 決定: 既定ブランチの先端だけ（2026-11-23） |
| Q6 | Wiki のページの ID | **`WIKI-0001` からの連番**（画面が次の番号を付ける。題名は日本語でよい） | 決定: `WIKI-0001` からの連番（2026-11-23） |
| Q7 | 検索の最短の語 | **2 文字**（1 文字は `FG-WEB-0006` と「2 文字以上で」の案内） | 決定: 2 文字（1 文字は `FG-WEB-0006`）（2026-11-23） |
| Q8 | 社内の GitLab の Wiki を移すか（事実が要る） | 移す道具は作らない。必要なら `.wiki.git` を clone して Wiki のプロジェクトへ push し、`gstn doc fmt` で frontmatter を足す手順を runbook `knowledge` に書く | 決定: 移す道具は作らず、手順を runbook `knowledge` に書く（2026-11-23） |
