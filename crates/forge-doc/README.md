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

依存の向き（CLAUDE.md の契約 9）: `forge-core → forge-doc → forge-errors`、`forge-client → forge-doc`。依存してよいのは `forge-errors` と
純粋な部品（今は `yaml-rust2`）だけで、`tests/purity.rs` が確かめる。

段 2 の MR1 は **移動だけ**（振る舞いを変えない）。MR2a で型の定義・新しい形式の解析・lint を足した。
MR2b で `fmt`、MR2c で旧い形式の読み替え（`legacy`）を足した。
残り: `spec_doc` の本文の読み取りを新しい解析に置き換える（`legacy` の比べる試験が条件。意図した違いは `legacy.rs` の先頭）、`gstn doc` と `forge/doc-lint` とエラーコード `FG-DOC-*`（MR3。
コードは、それを出す境界と一緒に登録する）。他のリポジトリへの参照（`[[repo:ID]]`）は呼び出す側が `.forge/repo.yml` で解決する。
