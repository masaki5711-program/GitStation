# ADR-0008: 保護ブランチの判定 — 事前の検査と pre-receive フック（ポリシーのスナップショット）

**Date**: 2026-10-26
**Status**: accepted
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## Context

保護ブランチ・保護タグ（force push 禁止、削除禁止、push できるロール）を確実に守る必要がある。
receive-pack のコマンドだけでは、オブジェクトを受け取る前なので fast-forward かどうか判定できない。
ref ごとの `update` フックでは、複数の ref の push が一部だけ反映される。

## Decision

- **(a) 事前の検査**: `serve` が receive-pack のコマンド（ref 名、旧・新の oid）を読み、
  「このロールはこの ref に push できない」「保護 ref の削除」をパックの受信前に拒否する。
- **(c) 最終判定は `pre-receive`**: フックは `forge hook pre-receive` を実行し、`serve` が認可のときに一時ファイルへ書いた
  **ポリシーのスナップショット** を読む。隔離環境の中で `git merge-base --is-ancestor` により fast-forward を判定する。
  pre-receive は push 全体を一括で拒否する（一部だけ反映されない）。フックは DB に接続せず、秘密情報も持たない。
- `post-receive` は反映された `旧 新 ref` を一時ファイルに書き、`serve` が git の終了後に監査ログと Outbox を 1 つのトランザクションで書く。

## Alternatives Considered

### 事前の検査だけ
- **Why not**: fast-forward を判定できない

### リポジトリ全体の `receive.denyNonFastForwards`
- **Why not**: ref ごとの保護を表現できない

### `update` フック
- **Why not**: 複数の ref の push が一部だけ反映される

## Consequences

### Positive
- 安価な拒否（パック受信前）と、確実な一括判定の両方が得られる

### Negative
- フックのスクリプト（`#!/bin/sh` の薄いラッパー）の配置と検証が要る

### Risks
- ref の更新後に DB が落ちると監査とイベントが残らない → `FG-GIT-0012` で ref の一覧をすべてログに残し、再投入の Runbook を用意する

## Amendment（2026-10-31、手順 D4 MR1: 保護の規則の表とパターン）

- 規則は `forge.protected_refs`（`project_id`、`pattern`、`push_min_role`）。`push_min_role` は `developer` / `maintainer` / `owner`
  （Guest と Reporter はもともと push できないので、規則の値にしない）。プロジェクトの監査の行で覆う。
- **パターン**（`forge-core` の `RefPattern::parse`、違反は FG-REPO-0013 `ProtectedRefRuleInvalid`）。次の順で調べ、最初の違反を返す。
  1. 255 バイトまで（`too_long`。長さは文字数ではなくバイト数）。
  2. `refs/heads/` か `refs/tags/` で始まる（`prefix`。大文字小文字を区別する）。
  3. `*` は 1 つまで、最後のパスの部分の中だけ（`wildcard`。`refs/heads/release/*` は可、`refs/heads/*/x` は不可）。
  4. `*` を普通の文字とみなして、`RefName` の規則（git の check-ref-format と、`-` で始まる部分の禁止。`check_ref_format`、失敗した規則を `rule`）。
- **照合**（`RefPattern::matches`）は、両側を同じ規則で畳んで比べる（大文字小文字を区別しないファイルシステムでは `MAIN` と `main` が
  同じファイルになるため）。畳み方は 1 文字ずつで、前後の文字に依らない（`char::to_lowercase` の後、語末の `ς` を `σ` にする）。文字列全体の
  `to_lowercase` は語末の `Σ` を `ς` にするので、パターンと ref で畳み方が変わり、守られない ref が出る（レビューの HIGH）。
  Unicode の正規化（NFC と NFD）は行わない。1 つの符号位置の `é` と、`e` + 結合文字は、別のパターン・別の ref である。`*` は空を含む任意の文字の並びに一致し、`/` もまたぐ（`refs/heads/release/*` は `refs/heads/release/1/hotfix` も守る）。
  一致を広げても、守られる ref が増えるだけである（一致した規則のうち最も厳しいものが効く。`RefProtection::effective`）。
- **DB の CHECK は接頭辞と長さだけ**（`protected_refs_pattern_prefix`、`protected_refs_pattern_length`）。残りの規則はパーサーだけが持ち、書き手は
  必ず `RefPattern::parse` を通す。DB が拒否するのは、パーサーが `prefix` か `too_long` を返すときに限る（プロパティテスト
  `the_db_check_agrees_with_the_ref_pattern_parser`）。
- **重複**: 同じプロジェクトで、大文字小文字だけが違うパターンは 1 つまで。DB の裏付けは一意の索引 `protected_refs_project_pattern`
  `(project_id, lower(pattern))` で、`pattern` は `COLLATE "C"` なので `lower()` は ASCII だけを畳む（サーバーのロケールに依らない）。
  ASCII 以外の大文字小文字の違いは、D4 MR5 の書き込みが `RefPattern::key()` で調べる（`duplicate_pattern`）。規則の数の上限
  （1 プロジェクト 100、`too_many_rules`）も MR5 で、アプリケーションだけが守る（DB には上限がない）。

## Amendment（2026-10-31、手順 D4 MR5: 規則の書き込みと読み込み）

- 規則の書き手は `forge_db::protected_refs::{create, change, delete}` だけ。どれもプロジェクトの行を `FOR UPDATE` で取ってから既存の規則を読むので、
  規則の数（100 まで、`too_many_rules`）と重複（`RefPattern::key()` で比べる。ASCII 以外の大文字小文字も、`duplicate_pattern`）は確定した規則に対して調べられ、
  同時の作成で破れない。`push_min_role` に Guest / Reporter は使えない（`FG-REPO-0013 push_min_role`）。`change` は `push_min_role` だけを変える
  （パターンを変えるには、消して作り直す）。監査の行はプロジェクト（`project` / id、`project_id` あり）、イベントは `protected_ref.*`（分類 `maintainer`、
  ADR-0016 の D4 MR5 の Amendment）。
- **ポリシーのスナップショットは `protected_refs::levels_for(db, ProjectId, &RefName)` から作る**。これは、そのプロジェクトの規則のうちパターンが ref に一致する
  **すべて** の `push_min_role` を返し（`RefPattern::matches`、大文字小文字を区別しない）、`RefProtection::effective` が最も厳しいものを選ぶ。最初に一致した規則だけを
  使う実装にはしない（規則を足しても保護が緩まないことを、forge-web のテスト `adding_a_rule_never_loosens_protection` が DB から確かめる）。
  読み込みは読み取り用のプールの 1 文で、行は 101 までに限る。100 を超える行、読めないパターン・ロールは規則を飛ばさずに `FG-DB-0020`
  ProtectedRefDataInvalid（alert、Runbook `protected-ref-data`）で拒否する。
