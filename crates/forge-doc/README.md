# forge-doc

文書の解析（計画 13、ADR-0026 §8）。**純粋**: I/O・時計・乱数を使わない（バイトを受け取り、値を返す）。
サーバー（`forge-core` → `forge-checks` の `forge/spec-lint`、段 2 の `forge/doc-lint`）とクライアント `gstn`（段 2 の `gstn doc fmt` / `lint`）が
同じ解析を使うため、`forge-core` から切り出した。

| モジュール | 中身 |
|---|---|
| `yaml` | 厳しい YAML の木（アンカー・エイリアス・タグ・2 つ目の文書・重複の鍵を断る。値は書いたとおりの文字）。`.forge/repo.yml` と前付けが使う |
| `spec_doc` | 仕様の文書（前付け・`::: req` のブロック）の解析。`forge_core::spec_doc` として今までどおり使える |

依存の向き（CLAUDE.md の契約 9）: `forge-core → forge-doc → forge-errors`、`forge-client → forge-doc`。依存してよいのは `forge-errors` と
純粋な部品（今は `yaml-rust2`）だけで、`tests/purity.rs` が確かめる。

段 2 の MR1 は **移動だけ**（振る舞いを変えない）。新しい形式・`fmt`・lint・型の定義は MR2 で足す。
