# 引き継ぎ — X3 の結びつけの確認の画面・文書の形式 段 2 MR0/MR1（2026-11-10・クラウド）

前の引き継ぎ: `2026-11-09-document-format-stage0.md`、`2026-11-09-kanflow-k4.md`。ブランチ `claude/clever-gates-j3elqg`（GitStation・
エージェントモデル）。

## 1. やったこと

### GitStation
- `8c6a877` **結びつけの確認の画面**（ADR-0025 Amendment X3 の 4 で後回しにしていたもの。kanflow の X3 の前提）:
  `GET /api/v1/projects/{id}/integration-bindings/act-as-confirmation`（`ActAsConfirmView`、本人の Web セッションだけ、forge の事実だけ、
  厳しい問い合わせ）と画面 `/-/confirm/projects/{id}/integration-bindings`（MR の確認の画面と同じ守り）。権限の表に 10 行。
- `1c139ab` 独立レビュー（security）の HIGH を直した: 問い合わせの値を百分率の復号をせずに比べていた（`:` が `%3A` で届くと 400）。
  残した MEDIUM / LOW は ADR-0025 の末尾の節に書いた（押せない人への 403 と 404 の違い、403 の文が「この人では行えない」だけ）。
- `bb5e9ce` **計画 13 段 2 MR1**: 純粋なクレート `forge-doc`（`yaml` = 厳しい YAML の木、`spec_doc` = 仕様の文書の解析）を作り、
  forge-core からそのまま移した（振る舞いを変えない。`forge_core::spec_doc` は再輸出、`RepoConfigProblem` は `forge_doc::yaml::YamlProblem`
  の別名）。層の表（`forge-hosting/tests/dependency_direction.rs`）に `forge-doc`、`forge-client` は `forge-doc` も可。`forge-doc/tests/purity.rs`。
  製品の CLAUDE.md の契約 9 と README。**本当は MR ごとにブランチを分ける計画（§3.4）だが、このセッションは指定のブランチ 1 本で進めた。**

### エージェントモデル（GitStation_Agent）
- **計画 13 段 2 MR0**: CLAUDE.md の契約 9 に `forge-doc` と `gstn` の依存の規則、変更履歴 v0.5.12。

### Kanflow v2（`/home/user/kandlowver2`、GitHub `origin`）
- `feat/L4-NOPBI-mr-awaiting-approval`（`96a6796` + レビューの修正 `af42f75`）を push 済み: ダッシュボードの「承認を待っている MR」。
- X3（リポジトリの結びつけを本人として）は作業中（ブランチ `feat/L4-NOPBI-repo-binding-act-as`、push はまだ）。

## 2. 確かめたこと

- `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`: 緑。
- `gen-authz --check`、`gen-catalog --check`: 最新。
- `cargo test -p forge-auth -p forge-web -p forge-client`: 745 件緑（8c6a877 の時点）。`binding_routes` 4/4、`web_ui_tests` 10/10（1c139ab）。
- `cargo test -p forge-doc -p forge-core -p forge-checks` と `forge-hosting --test dependency_direction`: 緑（bb5e9ce）。
- web: `npm run typecheck`・`lint`・`vitest` 810 件緑。
- **流していない**: ワークスペース全体の `cargo test`（ディスクが足りず、forge-db 等の全体は bb5e9ce の後に未実行。移動だけなので
  コンパイルは clippy の all-targets で通っている）。

## 3. 次にやること

1. 段 2 MR2（`feat/core-doc-format`）: 新しい形式・`fmt`・型の定義・lint・FG-DOC-0001/0003/0004、合成の例を試験の入力に。
2. Q13（まだ GitStation に無い文書への参照）と Q14（機密の違う文書への参照）の答え — MR3 の前に要る。
3. kanflow X3 を仕上げてレビュー、push（利用者の指示の後）。
4. ADR-0025 の残り: 公開の経路で代理のヘッダーを断る（MEDIUM）、LOW 4 件、確認の画面の 403 の文。
5. ディスク: `target/debug/deps` の古いテストの実行ファイルが溜まると容量が尽きる。
   `find target/debug/deps -maxdepth 1 -type f -perm -u+x ! -name '*.so' -delete` で空ける。

## 4. 追記（2026-11-10 の後半）

利用者の決定（推奨で）: Q13 = (b) 移行中は期限つきで警告に下げる（DB の設定）、Q14 = 機密の高い参照先は ID だけ、確認の画面は押せない人にすべて 404。
記録: 計画 13 §14 の末尾、ADR-0026 の末尾、ADR-0025 の末尾（`9cd5b7d`）。

| コミット | 中身 |
|---|---|
| `428e505`・`5073772`・`450f529`・`b6a81a3` | 段 2 MR2a（型の定義・解析・lint）、レビューの修正、MR2b（`fmt`）、`fmt` のレビューの修正 |
| `401ff4f` | MR2c: 旧い形式（`spec:`）を新しい解析で読む（`legacy`、生成した文書で新旧の要件が一致する試験） |
| `a2166ac`・`ed0a879` | MR3: `gstn doc fmt` / `lint`、`forge/doc-lint`（`check.spec_index` の中）、FG-DOC-0001〜0004、レビューの修正 |
| `5db4636`・`aa640ec` | ADR-0025 の残りの指摘（公開の経路で代理のヘッダーを断る、切れた要求の主張を捨てる、鍵の消去、確認の画面の題名） |
| `ff17e86`・`423ca54` | 確認の画面は押せない人にすべて 404（`authorize()` の 1 か所、ログに `refused_code`）、代理実行が未設定のときも同じ 404 |
| `b5389b9`・`1d4bcb5`・`2a0b96c` | 画面: 連携の管理・プロジェクトの結びつけ（連携ごとに 1 行、解除は全部を一覧）・MR のチェックの結果と衝突の予測、結びつけの設定（コード / 共有の仕様）を API と画面に |

Kanflow v2: K4・承認待ちの欄・X3 を `main` に統合（`49720ae`）。見積もりのパラメータの探索はブランチ `feat/L4-NOPBI-plan-param-search`
（`254c2e6`・`7d979bf`、レビューの修正済み、`main` には未統合）。

**次にやること**
1. ~~Q13 のリポジトリの設定（追記のマイグレーション、Action と権限の表の行、ルートと `gstn`、`forge/doc-lint` で `ref_broken` を警告に、期限の表示）。~~
   **済み**（ブランチ `claude/clever-gates-j3elqg`）: 表 `doc_lint_migrations`（20261203100000）、`DocLintMigrationManage`（人だけ、Maintainer）、
   `GET|PUT|DELETE …/doc-lint/migration`、`gstn doc migration`、`FG-DOC-0005`（期限は今日より後、180 日以内。今日は DB の日付）、
   設定の画面の「移行中の文書の検査」。`docs/api/checks.md` の「移行中の警告」。
2. `gstn doc fmt --migrate`、`spec_doc` の本文の読み取りを新しい解析に置き換える（`legacy` の比べる試験が条件）。
3. ~~段 3（表示と文書の JSON）。Q14 の「ID だけ」はここで実装。~~
   **済み**（ブランチ `claude/clever-gates-j3elqg`）: `GET /api/v1/projects/{id}/documents[?ref=&doctype=&limit=&after=]` と
   `GET …/documents/{doc_id}[?ref=&lang=]`（`RepoBrowse`、`forge-web/src/routes/documents.rs`）。木は `forge_checks::doc_lint::read_tree`
   （lint と同じ読み方と上限、上限に当たると `incomplete` FG-DOC-0002）。索引は lint と同じ（`doctype` を書き `<id>.md` の名前、同じ id は
   パスの順で最初、訳は `<id>.<lang>.md`）。型が読めない文書は `confidential`。参照先の方が機密なら `hidden`（ID だけ、題名を送らない）。
   AI エージェント（`audience`）には機密の文書を FG-AUTH-0021 `doc_confidential` で断り、一覧からは外して `withheld` に数える
   （読んだ版か既定のブランチの先で機密なら）。無い文書・訳は見えないプロジェクトと同じ 404（FG-REPO-0005、ログに
   `document_not_found` / `translation_not_found`）。表示用の本文は `forge_doc::render::display_markdown`（散文の行だけを和文の詰めで
   つなぐ。表・見出し・コードはそのまま）。画面の語 `documents`（`RouteWord::Documents`、`/-/documents`・`/-/documents/{id}`）、
   `gstn doc show` / `gstn doc list`、Web の文書の画面と一覧（ファイルの画面に「文書として開く」）。
   **残り**: blob / raw の経路は計画 13 の `classification: confidential` を見ない（`marked_paths` は仕様のディレクトリの
   `confidential:` だけ）。MCP の道具（文書を ID で取る）・逆引き・型の名前（`label`）の表示は未実装。
4. Kanflow: パラメータの探索を `main` へ（利用者の指示の後）、AI が実績を読んで学習から外す候補を出す案（引き継ぎ 2026-11-05 の (1)）。
5. PC 側: ワークスペース全体の試験、M3 の完了の条件、計画 13 の段 1。

**注意**: このクラウドの容器はディスクが約 10 GB しか空かない。並行の Rust のビルドで満杯になり、開発用の PostgreSQL が 2 回止まった。
`find target/debug/deps -maxdepth 1 -type f -perm -u+x ! -name '*.so' -delete` と `rm -rf target/debug/incremental` で空ける。
Kanflow の `origin` の URL は旧い名前（GitHub が `KandlowVer2` に転送）。Kanflow のコミットの作成者は CLAUDE.md の規則どおり利用者の名前
（GitHub では署名が無いので Unverified と出る）。
