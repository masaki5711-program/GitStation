# forge-doc

文書の解析（計画 13、ADR-0026 §8）。**純粋**: I/O・時計・乱数を使わない（バイトを受け取り、値を返す）。
サーバー（`forge-core` → `forge-checks` の `forge/spec-lint`、段 2 の `forge/doc-lint`）とクライアント `gstn`（段 2 の `gstn doc fmt` / `lint`）が
同じ解析を使うため、`forge-core` から切り出した。

| モジュール | 中身 |
|---|---|
| `yaml` | 厳しい YAML の木（アンカー・エイリアス・タグ・2 つ目の文書・重複の鍵を断る。値は書いたとおりの文字）。`.forge/repo.yml` と前付けが使う |
| `spec_doc` | 仕様の文書（前付け・`::: req` のブロック）の解析。`forge_core::spec_doc` として今までどおり使える |
| `doctype` | 文書の型の定義 `.forge/doctypes/<型>.yml` の読み込み（項目・見出し・ブロック・属性の型・ID の形・型の規則。統制は書けない） |
| `document` | 計画 13 の形式の文書の解析（前付けをそのまま、見出し、ブロックと引用符の値、`[[ ]]` の参照、HTML の禁止）。分類は閉じる側に倒す |
| `legacy` | 旧い形式（`spec:` の前付け）を新しい解析で読む: 型 `spec.system`、ID は `spec` の値、要件は `spec_doc` と同じ答え（生成した文書で両方の答えを比べる試験） |
| `lint` | 型と他の文書に照らした検査（必須の項目と見出し、ID の形と重複、壊れた参照、型の規則）。合成の例の README の期待値と完全に一致 |
| `fmt` | 整形（1 文 1 行、前付けの項目の順、ブロックの属性の順）。冪等。旧い形式・`doctype` の無いファイルは整形しない |

## 使う側（段 2 の MR3）

同じ `lint` と `fmt` を、同じバイトに対して 2 か所で使う。答えは一致する。

| 使う側 | 何を読むか | 結果 |
|---|---|---|
| `gstn doc lint` / `gstn doc fmt`（`forge-client` の `doc.rs`） | 手元の作業ツリー（上へたどって見つけたリポジトリ。`git ls-files` で一覧、`.gitignore` を守る）。サーバーには何も送らない | lint: 誤りで終了コード 1（`FG-DOC-0001`、型の定義が使えないと `FG-DOC-0003`）。fmt `--check`: 変わるファイルがあると 1（`FG-DOC-0004`） |
| `forge/doc-lint`（`forge-checks` の `doc_lint.rs`） | MR の版の head。`check.spec_index` のジョブの中で、`forge/spec-lint` と **同じ 1 回の木の一覧と読み取り**から作る | `success` / `failure`（`FG-DOC-0001` / `0003`）/ `neutral`（上限: `FG-DOC-0002`） |

- 文書は、前付けが `doctype` を書いている `.md`（どのフォルダでも）。型は `.forge/doctypes/<型>.yml`。
- 上限: サーバーは木の一覧 50,000 件（超えても仕様のディレクトリは別に一覧するので `forge/spec-lint` の答えは変わらない）、`.md` 2,000 個、解析の上限を超える大きなファイル 100 個、合計 32 MiB、型 64 個。`gstn` は `.md` 5,000 個、合計 64 MiB、一覧 16 MiB、型 64 個。
  文書は解析の上限（1 MiB）＋1 バイトまで読む（`too_large` の答えをそろえるため）。上限に当たったら黙って通さず `FG-DOC-0002`。
- 壊れた参照（`ref_broken`）は誤り（計画 13 Q13）。`gstn doc lint --migration-warnings` だけが手元で警告に下げる。
  サーバー側の「移行中は警告」のリポジトリ設定（期限つき、DB、`authorize()`）は **まだ作っていない**。

依存の向き（CLAUDE.md の契約 9）: `forge-core → forge-doc → forge-errors`、`forge-client → forge-doc`。依存してよいのは `forge-errors` と
純粋な部品（今は `yaml-rust2`）だけで、`tests/purity.rs` が確かめる。

段 2 の MR1 は **移動だけ**（振る舞いを変えない）。MR2a で型の定義・新しい形式の解析・lint を足した。
MR2b で `fmt`、MR2c で旧い形式の読み替え（`legacy`）を足した。
MR3 で `gstn doc fmt` / `lint`、`forge/doc-lint`、エラーコード `FG-DOC-0001`〜`0004` を足した（上の表）。
残り: `spec_doc` の本文の読み取りを新しい解析に置き換える（`legacy` の比べる試験が条件。意図した違いは `legacy.rs` の先頭）、
Q13 のリポジトリの設定。他のリポジトリへの参照（`[[repo:ID]]`）は呼び出す側が `.forge/repo.yml` で解決する。
