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
- （D4 のレビュー）`delete` は行を解釈せずに取る（同じプロジェクトの中だけ）。読めない行（FG-DB-0020）も消せるようにするため（Runbook `protected-ref-data`
  の修復の手順）。監査の詳細は保存されていた文字列（パターンは 255 バイト、ロールは 32 文字まで）と `stored_invalid: true`。保存された行が 100 を超えるときは、
  `create` も `too_many_rules` ではなく FG-DB-0020 `too_many_stored_rules` を返す。

## Amendment（2026-11-01、M1 段階 9 MR2: 規則のスナップショットと push の記録）

- **スナップショット**: push は規則を 1 回だけ読む。`forge_db::protected_refs::rules_for(db, ProjectId)` が、そのプロジェクトの
  すべての規則を `forge_core::ProtectionRules` として返し（読み取り用のプールの 1 文、行は 101 まで。101 行目は FG-DB-0020
  `too_many_stored_rules`、読めない行も FG-DB-0020。規則を飛ばさない）、`ProtectionRules::levels(&RefName)` が各 ref の
  レベルを返す。これは `levels_for` と同じ答えである（同じ SQL と解釈。プロパティテスト `rules_for_matches_levels_for`）。
  forge-web は `forge_core::ProtectionLoader` を通して読む（forge-db に依存しない）。1 つの push の ref はすべて同じ
  規則で判定されるので、途中で規則が変わっても一部の ref だけに効くことはない。
- **記録**: §Risks の「`FG-GIT-0012` で ref の一覧をすべてログに残し、再投入の Runbook を用意する」は、ログの行
  `forge.git.push_applied`（`forge_core::PushApplied`）、`FG-GIT-0012` PushRecordFailed、Runbook `push-record-failed`、
  `forge admin repo record-push --from-log` で実現した。再投入の鍵は ADR-0016 の M1 段階 9 MR2 の Amendment
  （`repository_pushed:<repo_id>:<sha256(request_id ‖ 名前順の更新)>`）。

## Amendment（2026-11-01、M1 段階 9 MR4: 案 B — 事前の検査と git 自身の拒否。フックは M3 へ）

Smart HTTP の push（`POST {path}.git/git-receive-pack`）は、Decision の (c)（pre-receive フック）を **使わずに** 実装した。
フックがない・実行できないとき git はそれを黙って飛ばす（判定が素通りになる、fail-open）ため、検証の仕組みのないまま
フックに頼るのはやめ、段階 9 では次の **案 B** を採った（案 A「pre-receive を今入れる」は不採用）。pre-receive と
`post-receive` は **M3（段階 10）** で、フックが確かに動いたことを検査する仕組みと一緒に入れる。

- **認可の順序**: プロジェクト単位の `RepoPush`（Developer 以上、`write_repository`）はディスパッチャーが本文を読む **前** に
  判定する（権限のない呼び出しには `100 Continue` も返さない）。
- **(a) 事前の検査（拡張）**: ハンドラーがコマンド部（≤ 1000 件、≤ 1 MiB、`report-status` 必須、`push-cert` 拒否）を読み、
  各 ref を `RefName::parse`（FG-GIT-0005）と 16 進の名前の検査（FG-GIT-0011 AmbiguousRefName: ブランチ名・タグ名が
  40 / 64 桁の 16 進）に通す。規則は push ごとに **1 回** 読み（`ProtectionLoader`、MR2 のスナップショット）、各更新を
  分類する: 旧が 0 なら作成、新が 0 なら削除、既存の **タグの更新はすべて force**、保護ブランチで新しいコミットが
  すでにリポジトリにあれば `rev-list --count --max-count=1 <old> ^<new>` で判定、それ以外は fast-forward として扱い
  最終判定を git に任せる。各 ref は `forge_auth::authorize_ref_updates`（プロジェクトの証明を受け取り、ref ごとに
  `authorize()` を呼ぶ。判定点は 1 つのまま）で判定する。ハンドラーは `authorize()` を直接呼ばない。
- **一括の拒否**: 1 つでも拒否があれば git を起動せず、本文を読み捨て（最大 64 MiB）、push 全体を帯域内で拒否する:
  `unpack ok`、拒否された ref は `ng <ref> <FG コード> <利用者向けメッセージ>`、他の ref は `ng <ref> atomic push failure`、
  サイドバンド 2 に `<FG コード> <メッセージ> (request_id: …)`。どの ref も変わらない。
- **(c') git 自身の拒否**: 保護ブランチを更新する push には `-c receive.denyNonFastForwards=true` を付ける（新しいコミットは
  パックで届くので、fast-forward かどうかはパックの後でないと分からない）。git の `ng <ref> non-fast-forward` は、
  保護 ref については FG-GIT-0007 の文言に書き換えて返す（`reason=git_denied_non_fast_forward` をログ）。
  この経路の拒否は ref ごと（git の通常の push と同じ）で、同じ push の他の ref は反映されうる。また
  `denyNonFastForwards` は push 全体に効くので、保護ブランチを含む push では保護されていないブランチの force push も
  git が拒否する（別の push に分ければ通る。既知の制限、M3 の pre-receive で解消）。
- **git に渡すもの**: 展開済みの元のバイト列をそのまま（コマンド部もパックも）。`receive.fsckObjects`、
  `receive.maxInputSize = git.http.push_max_bytes`（既定 2 GiB）、空のフックディレクトリ（`core.hooksPath`）は MR1 の固定設定。
- **記録**: 本文を受け取り終えた receive-pack はリクエストの持たないタスクで動く（ref 更新の途中でクライアントが切れても
  殺さない）。git の終了後、report-status で `ok` の ref について種類と新しいコミット（ref ごとに 20 件まで、すべて数える、
  push 全体で 10 秒。超えたら `total_commits_partial`、件数は数えた分＝下限）を読み、**先に** `forge.git.push_applied` を
  ログに出してから `record_push` を 30 秒以内に 3 回まで試す（`Deduped` は成功）。すべて失敗したら FG-GIT-0012。
- **レビューを受けて（同日、MR4 の追加のコミット）**:
  - 証明は判定した主体を持つ（`Authorized` の非公開の欄）。`authorize_ref_updates(&Authorized<ProjectCtx>, &[RefUpdateCtx])` は
    主体を引数に取らず証明の主体で判定する（別の主体と組み合わせられない）。規則のスナップショットは読み込んだプロジェクトを
    持ち（`ProtectionRules::for_project`）、証明のプロジェクトと違えば FG-SYS-0001 `rules_project_mismatch`。
  - 保護ブランチを更新する 2 件以上のコマンドの push には、forge が最初のコマンドに `atomic` を足す（git の
    `denyNonFastForwards` の拒否が ref ごとなので、他の ref だけが反映されることを防ぐ）。レビューの推奨案「祖先が分からない
    保護ブランチの更新を force とみなして拒む」は採らなかった: 新しいコミットは普通パックで届くので、保護ブランチへの
    通常の fast-forward がすべて拒まれる。上の「保護されていないブランチの force も拒まれる」制限は、この atomic で
    「push 全体が拒まれる」になる。
  - git の答えが読めない・git が答えの前に失敗した・本文の途中で殺した、のどれでも、コマンドのすべての ref をリポジトリから
    読み（`Git::ref_values`）、新しい値になっている（削除なら無い）ものを反映済みとして記録する。読めなければ
    FG-GIT-0012 `reason=reconcile_failed` に全コマンドを 1 回で出す。
  - `forge.git.push_applied` は git の終了直後に `stage=minimal`、種類とコミットを読んだ後に `stage=final` を出す。
  - git の `unpack` の文言、短い既知の理由以外の `ng` の理由、サイドバンド 3 は FG-GIT-0014 と request_id に置き換え
    （元の文はログに 1 回、`reason=git_refused`）、サイドバンド 2 からは保存先のパスを消す。
  - 受け取り終えた receive-pack のタスクは `SmartHttp::push_tasks` で数え、`forge serve` は停止時に最大 90 秒待つ
    （超えたら FG-SYS の `ServeFailed operation=push_tasks_pending`）。
  - 権限表: `write_repository` / `api` / `read_repository` のトークンと保護 ref、エージェントのトークン（`is_agent`。ADR-0017 の
    エージェントの制限はマージの系統だけなので、push は人間のトークンと同じ判定。`is_agent` はまだ認可の入力ではない、W1）、
    保護タグの作成と force の行を足した。

## Amendment（2026-11-01、M1 段階 8b MR8: 規則の API と重複のコード）

- 規則の一覧・作成・変更・削除を `GET|POST /api/v1/projects/{id}/protected-refs`、`PATCH|DELETE …/{rule_id}` で公開した
  （`ProtectedRefList` / `ProtectedRefManage`）。変更の `from`（いまの最小 push 役割）はルートの読み込みと、書き込みの
  トランザクションの中で規則の行を `FOR UPDATE` で読み直した値で判定する（依頼の値は使わない）。読めない行は Owner の水準として
  判定し、Owner か管理者だけが消せる（runbook `protected-ref-data` の手当て）。
- 同じパターン（`RefPattern::key()`）の規則の追加は FG-REPO-0013 `duplicate_pattern`（400）から **FG-REPO-0017
  ProtectedRefPatternTaken（409）** に変えた（データベースの一意インデックスでの検出は `reason=unique_index`）。依頼の形の誤りではなく
  既存の規則との衝突なので、クライアント（gstn）が区別できるようにする。
- 規則の削除と push の判定が同時に起きる場合は扱わない: push は git を動かす前に読んだ 1 つのスナップショットで全 ref を判定する
  （この ADR の方針どおり、スナップショットの後に確定した削除はその push に効かない）。
