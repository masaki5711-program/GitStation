# ADR-0026: 文書の形式、文書の型の定義、解析のクレート `forge-doc`

**Date**: 2026-11-09
**Status**: proposed（計画 13 v0.5 の決定を ADR にした。**accepted は、PC 側で実際の文書 3 つをこの形式に写す試し（計画 13 §12 の段 1）の後**。
書式を変える指摘が出たら、この ADR と段 2 の実装を直す）
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## Context

利用者は、システム仕様だけでなく QMS の規程・安全衛生の文書・設備要求仕様（URS）も Markdown で GitStation に置き、エンジニア・非エンジニア・AI が
同じ文書を扱えるようにしたい（計画 13）。既に `forge_core::spec_doc` / `spec_index`（M3 X4、ADR-0023 Amendment X4）が `spec/` の構造化仕様を解析し、
`forge/spec-lint`・`forge/spec-trace`・仕様の API・文脈の API（X5）が使っている。その frontmatter は `spec`・`title`・`status`・`owners`・
`confidential` だけを受け付け、計画 13 の `doctype`・`id`・`owner` を拒否する。手元の PC で整形と検査をする `gstn`（ADR-0020）は `forge-errors`
にしか依存できず、`forge-core` の解析を使えない。解析を 2 か所に持つと、手元とサーバーの答えが食い違う。

## Decision

1. **形式**（計画 13 §2〜§4）
   - 拡張子は `.md`。ファイル名は文書番号（`QP-07-001.md`）。訳は `<id>.<lang>.md`（§4）。フォルダは人のための並べ方で、型の判定に使わない。
   - 先頭の YAML（frontmatter）の必須は `doctype`・`id`・`title`・`owner` の 4 つ。任意は `classification`・`review_cycle`・`related`・`lang`・
     `translation_of`・`retired` と、型の定義が足す項目。**それ以外の項目は lint のエラー**（今の `spec_doc` と同じ厳しさ）。
   - **版・承認者・承認日・改訂履歴・状態はファイルに書かない**（Git と MR とタグから導く。ADR-0028）。例外は廃止だけで、frontmatter の
     `retired: { date, replaced_by }`（廃止も承認された MR で行う）。
   - **文書であること** = frontmatter に `doctype` がある `.md`（どのフォルダでも）。旧い形式（§3）は `spec/` などの仕様の置き場所の中だけ。
2. **ブロック**（計画 13 §4.2 と §13 の 7。今の `spec_doc` の文法の上位の互換）
   - 開く行: `::: <種類> <ID> [<鍵>=<値> …]`。閉じる行: `:::`。種類は `[a-z][a-z0-9-]*`、鍵は `[a-z][a-z0-9_]*`。
   - 値は空白と `"` を含まない語（今の形式）、または `"…"`（`\"` と `\\` だけをエスケープ。和文は引用符の中で可）。同じ鍵の 2 回はエラー。
   - ID の形は種類ごと: `req` は今の `RequirementId`（大文字と数字の区切り、`PBI` の族は不可）。他の種類は型の定義の `id_pattern`
     （既定は `[A-Za-z0-9][A-Za-z0-9.-]{0,63}`）。ID は文書の中で一意、一度付けたら変えない。
   - **廃止はブロックの属性 `status=deprecated`**（今の `spec_doc` と 03 §4.2 の言葉。計画 13 §4.2 の `status=retired` は使わない — 2 つの言葉にしない）。
   - **入れ子**: 今の規則のまま — ID の無い入れ物（`::: note` など）は入れ子にでき、**ID のあるブロックの中に ID のあるブロックは置けない**（`block_nested`）。
     計画 13 §13 の 7「入れ子は作らない」は、この意味に改める（旧い文書を壊さないため）。
   - コードのフェンス（```` ``` ```` / `~~~`）の中の `:::` はブロックではない。
3. **参照**: `[[ID]]`・`[[ID#ブロック]]`。他のリポジトリは `[[<リポジトリ>:<ID>]]`（`.forge/repo.yml` で版を固定したもの。03 §4.4）。
   パスではなく ID で解決し、**壊れた参照は lint のエラー**。`[[ID]]` は元の文書（言語の接尾辞の無いもの）に解決する。
4. **整形**（`fmt`）: 1 文 1 行（`。`・`！`・`？` の後でだけ分け、`「」`・`『』`・`（）`・コード・リンク・表・見出し・frontmatter の中では分けない）、
   frontmatter の項目の順、ブロックの属性の順。**冪等**（`fmt(fmt(x)) = fmt(x)`）。表示の側は和文と和文の間の改行を詰める（段 3）。
5. **文書の型の定義** `.forge/doctypes/<型>.yml`（計画 13 §5）: 必須の項目・見出し・使えるブロック・ID の形・型の規則（決定的なものだけ）。
   **統制（承認者・承認のフェーズ・同時の編集の緩め・Kanflow からの承認・機密区分を下げる承認者）は型の定義に書かない**。書けるのは「推奨」
   （`approval_hint` など。リポジトリを作るときの初期値の提案）だけで、効くのはリポジトリの設定（DB、ADR-0027・0028）。
6. **機密区分は `classification`（`public` / `internal` / `confidential`）を正にする**。旧い `confidential: true` は `confidential`、`false` は
   `internal` と読む。**両方を書くのは lint のエラー**。文脈の API・MCP（06 D-5、X5）は解析が導いた値を使う。
7. **旧い形式（構造化仕様）を読み続ける**: `spec: X` の frontmatter は `doctype: spec.system`・`id: X` として扱う。`status: deprecated` は
   `retired`、ほかの `status` の値は捨てて助言（`status_in_file`、警告）を出す。`owners: [..]` は `owner` に読み替えない（個人のメールで、
   グループではない）— 旧い文書では `owner` を必須にしない。**`forge/spec-lint`・`forge/spec-trace`・仕様の API の答えを変えない**
   （今の forge-core・forge-checks の spec の試験を新しい解析でそのまま通すのが置き換えの条件）。`gstn doc fmt` は旧い形式のファイルを
   書き換えない（`--migrate` のときだけ新しい形式に直す）。
8. **解析は新しい純粋なクレート `forge-doc` の 1 か所**: 解析・整形・lint・型の定義の読み込み。I/O なし。依存は `forge-errors` と、
   I/O の無い部品（`yaml-rust2` など）だけ。今の `repo_config` の YAML の木の読み取り（アンカー・エイリアス・タグ・重複の鍵を拒否）も
   ここへ移し、`forge-core` が使う。
   - 依存の向き: `forge-core → forge-doc → forge-errors`、`forge-client（gstn）→ forge-doc → forge-errors`。サーバー側のどのクレートも
     `forge-client` に依存しない（変えない）。
   - **ADR-0020 の Amendment**: `gstn` が依存してよいサーバー側のクレートは `forge-errors` と `forge-doc`。理由: 手元で整形・検査し、CI の
     `forge/doc-lint` と同じ答えを出すため。`forge-doc` は I/O と秘密を持たないので、ADR-0020 の Cons（配布物の肥大、サーバーの内部の型）に当たらない。
   - **CLAUDE.md の契約 9 の文**を改める（製品のリポジトリと、エージェントモデルの写し。エージェントモデル側を先に入れる）:
     `forge-web / forge-cli → feature crates → forge-auth / forge-git / forge-db → forge-core → forge-doc → forge-errors`、
     `forge-client（gstn）→ forge-doc → forge-errors`。
9. **道具**: `gstn doc fmt` / `gstn doc lint`（手元。サーバーに送らない）、CI のチェック `forge/doc-lint`（MR の head。03 の `forge/spec-lint` を一般化。
   同じ `forge-doc`）。エラーコードは新しい領域 `FG-DOC-*`。
10. **今までの計画との食い違い**（計画 13 §10）:

| # | 食い違い | 決定 |
|---|---|---|
| 1 | 03 §4.2 の `spec:`・`status:`・`owners:` | 新しい形式に改め、旧い形式は 7 のとおり読み続ける。03 §4.2 に注記 |
| 2 | 07 §6（W12）の `checklist.yml` | 本文の `::: decision <ID> state=open\|decided\|na` ブロックにする。必須の項目が `open` の文書は、その型の規則 `decisions_resolved` で lint のエラー（「未決定の必須の項目があるまま公開できない」ゲートを MR の必須チェックで実現）。`checklist.yml` は作らない |
| 3 | `docs/specs/` と `spec/` | `spec/`（ADR-0023 のとおり）。09 §3 を改める |
| 4 | `.gitstation/` と `.forge/` | `.forge/`。09 §3 の `.gitstation/agent.lock.json` は `.forge/agent.lock.json` に改める |
| 5 | Web Studio と「画面の系統は 1 つ」（ADR-0023） | Web Studio は **リポジトリの画面の中の、文書を書く機能**（計画 13 §7）。別の画面の系統は作らない |
| 6 | 01 R3 の条件 | 「Git の基本を学んだ部署が、文書の改訂を画面だけで回せる」を R3 の条件の候補にする（計画 13 §12 の段 5 で確かめる） |
| 7 | W12 の出力（`export.yml`・Word のひな形） | 出力は最後の段（計画 13 §12 の段 8）。W12 の完了の条件のうち出力は段 8 まで外す |
| 8 | W12・計画 13 v0.1 の「承認者を型の定義に」 | リポジトリの設定（ADR-0028）。型は推奨だけ |
| 9〜13 | 既存のコード・ADR | 本 ADR の 6・7・8、ADR-0028 |

## Alternatives Considered

### 独自の拡張子・二重拡張子（`.gsd`、`.qms.md`）
- **Pros**: ファイル名で型が分かる
- **Cons**: 道具・AI・プレビューの既定の扱いが効かない。二重拡張子は型を 2 か所に持ち、Windows で壊しやすい（計画 13 §2.3）
- **Why not**: 文書番号の接頭辞が型を示し、`doctype` が正の 1 か所

### 新しい解析を足し、今の `spec_doc` を残す
- **Pros**: 今の仕様の試験と動きに触れない
- **Cons**: 同じ `:::` と frontmatter を 2 つの解析が読み、答えが食い違う
- **Why not**: 「解析は 1 か所」。置き換えの条件を「今の試験をそのまま通す」にして危険を抑える

### `gstn` はサーバーの API で整形・検査する（手元に解析を持たない）
- **Pros**: ADR-0020 を変えない
- **Cons**: 手元の PC で、オフラインで、push の前に検査できない。Claude Code の作業の輪が遅くなる
- **Why not**: `forge-doc` は純粋で小さく、依存の例外を 1 つ足す価値がある

## Consequences

### Positive
- 1 つの解析が、手元の `gstn`・CI の `forge/doc-lint`・仕様の索引・文脈の API・将来の画面と文書の JSON（段 3）に同じ答えを出す
- 旧い構造化仕様を壊さずに、新しい文書の型へ広げられる

### Negative
- `forge-core` の `spec_doc`・`repo_config` の YAML の読み取りを移す作業（使う場所が forge-checks・forge-cli・forge-db・forge-web にある）
- 契約 9 と ADR-0020 の文を 2 つのリポジトリで直す

### Risks
- 置き換えで spec-lint の答えが変わる → 今の spec の試験を変えずに通すことを条件にし、新旧の答えを比べる試験を足す
- 書式が実際の文書に合わない → この ADR は段 1 の後まで proposed。段 1 で書式を変える指摘が出たら、段 2 の実装を直す
- `classification` の読み替えの誤りで機密の文書が AI に渡る → 両方を書くのはエラー、読み替えの試験（`true` → confidential）を必須にし、
  文脈の API の既存の試験（X5）を新しい解析で通す

## 決まったこと（2026-11-10）

計画 13 §14 の Q13・Q14 は推奨で決まった（計画 13 §14 の末尾）。壊れた参照の「移行中は警告」はリポジトリの設定（DB）で、期限は必須
（ADR-0027・0028 と同じく統制は DB）。機密の高い文書への参照は ID だけを見せる。
