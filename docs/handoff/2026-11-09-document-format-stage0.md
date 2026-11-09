# 引き継ぎ — 文書の形式: 段 0（ADR の草案）・合成の例・段 2 の実装の計画（2026-11-09・クラウド）

前の引き継ぎ: `2026-11-09-document-format-plan.md`（計画 13 v0.5 まで）。ブランチ `claude/clever-gates-j3elqg`（`main` `ed56678` を取り込み、
計画 13 v0.5 を `claude/magical-thompson-c7ey6e` `2122c31` から取り込んだ）。**コードの変更はない。文書だけ。段 2 の実装は利用者の了承待ち。**

## 1. やったこと

- **ADR の草案 3 つ（すべて proposed）**
  - `0026` 文書の形式・文書の型の定義・解析のクレート `forge-doc`（ADR-0020 の Amendment と CLAUDE.md の契約 9 の文の改訂を含む。計画 13 §10 の
    食い違い 1〜13 の扱いの表）。
  - `0027` 同時の編集（改訂は文書ごとに 1 つ、期限つきの編集の宣言、緩めるのは DB の設定だけ）。
  - `0028` 電子の承認とフェーズ（DB の設定、再確認 = ADR-0021 の Amendment、承認した blob ＝ 発行する blob = ADR-0024 の Amendment、
    版のタグを正に、作成の署名は承認と別、トークンの承認の拒否）。
  - ADR-0020・0021・0024 の末尾に「Amendment（proposed）」の案内を足し、`docs/adr/README.md` に 3 行を足した。
  - **accepted の条件**: 実際の文書 3 つの試し（段 1、PC 側）。0028 はさらに品質保証の部門の確認（押印の代わりになるか）。
- **既存のコードとの突き合わせで決めた細部**（ADR-0026 §2。計画 13 も直した）: ブロックの廃止の言葉は今の `spec_doc` と 03 の **`status=deprecated`**
  （計画 13 §4.2 の `retired` はやめた）。入れ子は今の規則のまま（ID の無い入れ物は入れ子にでき、ID のあるブロックの中に ID のあるブロックは置けない）。
- **合成の例**（段 1 の代わり）: `docs/plan/13-examples/` — 型の定義 3 つ（`qms.procedure`・`safety.risk-assessment`・`equipment.urs`）と文書 3 つ
  （QP-07-001 規程・RA-2026-014 リスクアセスメント・URS-L3-001 設備要求仕様書。表・ブロック・引用符の和文の値・入れ子の note・廃止・
  `「…。」`・Mermaid・機密）。README に **期待する lint の結果**（わざと残したエラー 3 種）。段 2 の試験の入力にする。
- **計画 13 を直した箇所**: §4.2 の表に `step`・`record`、§4.2 の廃止の言葉、§13 の 7 の入れ子の文、**§14 新設**（例で見つけたこと: 決定 1〜7、質問 Q13・Q14）。

## 2. 利用者に答えてほしいこと（計画 13 §14）

- **Q13 まだ GitStation に無い文書への参照**（移行で必ず起きる）: (a) 常にエラー、(b) リポジトリの設定で「移行中は警告」に期限つきで下げる、
  (c) `.forge/external-docs.yml` に外の文書の一覧。**推奨 (b)**。段 2 の `forge/doc-lint` の挙動に関わるので、MR3 の前に要る。
- **Q14 機密の違う文書への参照**: 参照先の方が機密なら ID だけを見せ、題名・本文・AI の文脈に入れない（推奨）。表示（段 3）で要る。

## 3. 段 2 の実装の計画（了承待ち）

### 3.1 クレートと依存（ADR-0026 §8）

```
forge-web / forge-cli → feature crates（forge-checks …）→ forge-auth / forge-git / forge-db → forge-core → forge-doc → forge-errors
forge-client（gstn）→ forge-doc → forge-errors
```

- **`crates/forge-doc`**（新規、純粋。I/O・時計・乱数なし。依存は `forge-errors`・`yaml-rust2` だけ）:
  `yaml`（今の `forge_core::repo_config` の木の読み取りを移す）、`frontmatter`、`block`（文法・属性の値の型）、`refs`（`[[ ]]` の解析）、
  `doctype`（型の定義の読み込みと検査）、`document`（解析の結果の型）、`lint`（決定的な規則。型の規則 `every_hazard_has_control` など）、
  `fmt`（1 文 1 行・項目の順・属性の順。冪等）、`legacy`（`spec:` の旧い形式の読み替え）、`index`（複数の文書の id ↔ パス、参照の解決。純粋）。
  上限（ファイル数・バイト・ブロック数・問題の数）は今の `spec_doc` と同じ流儀で定数にする。
- **`forge-core`**: `spec_doc` を `forge-doc` の上の薄い層にし、`SpecDocument` などの今の API を保つ（forge-checks・forge-cli・forge-db・forge-web は
  変えない）。`repo_config` は `forge_doc::yaml` を使う。
- **`forge-client`**: `gstn doc fmt [--check] [--migrate] [<path>…]`、`gstn doc lint [<path>…] [--json]`。手元のファイルと `.forge/doctypes/` を読み、
  サーバーに送らない。registry・help の snapshot を足す（API のルートではないので C-1 の表には「ローカル」と書く）。
- **`forge-checks`**: `forge/doc-lint` を、今の `check.spec_index` のジョブの中で、同じ head の木から作る（木を 2 回読まない。新しいジョブの種類を作らない）。
  対象は frontmatter に `doctype` を持つ `.md`（どのフォルダでも）。結論: `success` / `failure`（FG-DOC-0001）/ `neutral`（上限に当たった: FG-DOC-0002）。
- **依存の向きの試験**を足す: `cargo metadata` を読み、`forge-client` と `forge-doc` の依存が許したものだけであることを確かめる（今はレビューだけで守っている）。

### 3.2 エラーコード（`crates/forge-errors/errors.toml`、`docs/errors/` は `gen-catalog` で生成）

| コード | 名前 | 使う場所 | 重さ |
|---|---|---|---|
| FG-DOC-0001 | DocLintProblem | `forge/doc-lint` の failure、`gstn doc lint`（FG-REPO-0028 と同じ流儀: 422、info、規則の名前とファイルと行） | info |
| FG-DOC-0002 | DocIndexBounded | 上限に当たり一部を読めなかった（neutral。FG-REPO-0031 と同じ流儀） | warn |
| FG-DOC-0003 | DoctypeDefinitionInvalid | `.forge/doctypes/<型>.yml` が読めない・形が不正（その型の文書はすべて検査できない） | info |
| FG-DOC-0004 | DocFormatWouldChange | `gstn doc fmt --check` で整形が要る | info |

アラートのコードは無い（runbook は要らない）。各コードの `test` に試験の名前を書く。

### 3.3 試験を先に書く

- `forge-doc`: 整形の往復と冪等（**合成の例 3 つを固定の入力にし、`fmt(例) = 例` のバイト一致**）、1 文 1 行の表（`「」`・`（）`・表・コード・リンク・
  見出しの中で分けない）、ブロックの文法（語の値・引用符・`\"`・重複の鍵・入れ子の規則・フェンスの中）、参照（壊れた・ブロック・同じ文書・
  他の文書・`replaced_by`）、訳のファイル名、`classification` の読み替え（`confidential: true` → confidential、両方はエラー）、
  型ごとに合格と不合格の例（各規則に 1 つ以上）、**例の README の「期待する lint の結果」と完全に一致**。
- **置き換えの条件**: 今の `spec_doc_tests`・`spec_index_tests`（forge-core）と forge-checks の spec の試験を **変えずに** 通す。旧い形式の文書の集まりで、
  新旧の解析の答え（要件・問題の一覧）が同じことを比べる試験を足す（MR1 の後、MR2 の中）。
- `forge-client`: `gstn doc fmt --check` の終了コード、`lint --json` の形、help の snapshot。
- `forge-checks`: `forge/doc-lint` の run（本物のリポジトリと DB。success / failure / neutral）。

### 3.4 MR の分け方（ブランチ名は `<type>/<area>-<topic>`）

| MR | ブランチ | 中身 | 評価役 |
|---|---|---|---|
| 0 | エージェントモデル `docs/harness-forge-doc-crate` | エージェントモデルの CLAUDE.md の契約 9 の文（**先に入れる**） | — |
| 1 | `refactor/core-forge-doc-crate` | `forge-doc` を作り、今の YAML の読み取りと `spec_doc` の解析を **そのまま移す**（振る舞いを変えない）。製品の CLAUDE.md の契約 9、依存の向きの試験 | `rust-reviewer` |
| 2 | `feat/core-doc-format` | 新しい形式（frontmatter・ブロックの一般化・参照・機密の読み替え・旧い形式の読み替え）、`fmt`、型の定義、lint の規則、FG-DOC-0001/0003/0004、合成の例を試験の入力に、型の定義 3 つ | `rust-reviewer`、`error-contract-auditor`、`security-reviewer`（機密の読み替えと AI の文脈）、`pr-test-analyzer` |
| 3 | `feat/cross-doc-lint` | `gstn doc fmt` / `lint`、`forge/doc-lint`（`check.spec_index` の中）、FG-DOC-0002、文書（crate の README・`docs/api` の検査の一覧） | `rust-reviewer`、`error-contract-auditor`、`silent-failure-hunter`、`pr-test-analyzer` |

- ゲート: `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、テスト（**クラウドには `cargo nextest` が無いので `cargo test` で代える**。
  変えたクレートを先に、マージの前にワークスペース全体を 1 回。2 つ同時に流さない）、`gen-catalog -- --check`、Windows の型の検査
  （`cargo check --target x86_64-pc-windows-gnu --tests`、mingw を入れて）。
- 型の定義 3 つの置き場所: 段 2 では `docs/plan/13-examples/.forge/doctypes/`（試験の入力）。リポジトリへ配る仕組み（ベースのリポジトリ・マーケットプレイス、W12・W15）は後の段。

## 4. PC 側で行う作業（利用者）

1. 計画 13（v0.5 と §14）と `13-examples/` を、エージェントモデル側の `docs/plan/` に写す。
2. **実際の文書 3 つ（規程・リスクアセスメント・URS 程度、機密でないもの）をこの形式に写す試し**（段 1）。書けない箇所・足りない型やブロック・表の崩れを記録し、
   ADR-0026〜0028 を accepted にするか、形式を直すかを決める。
3. 品質保証の部門に、電子の承認の案（計画 13 §6.3、ADR-0028）が押印の代わりになるかを確認する。
4. §2 の Q13・Q14 に答える。
