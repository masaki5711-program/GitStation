# ADR-0007: Smart HTTP は upload-pack / receive-pack を直接起動する（http-backend を使わない）

**Date**: 2026-10-26
**Status**: accepted
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## Context

Git のプロトコルは自前で実装せず `git` を実行する（CLAUDE.md §3-5）。実行の方法として、
`git http-backend`（CGI）を挟むか、`git upload-pack` / `git receive-pack --stateless-rpc` を直接起動するかを決める。
認可は起動の前に行い、失敗はサービスごとのエラーコードで記録したい。

## Decision

`forge-git` が `upload-pack` / `receive-pack` を `--stateless-rpc`（と `--advertise-refs`）で直接起動する。
広告の接頭辞（`# service=…`）、gzip の展開（上限付き）、ストリーミング、アイドルと全体のタイムアウト、
同時実行数の上限、終了コードからエラーコードへの対応を `forge-git` が持つ。
fetch / clone は `Git-Protocol` ヘッダを `GIT_PROTOCOL` として渡し protocol v2 に対応する。push は v0 / v1。

## Alternatives Considered

### git http-backend（CGI）
- **Pros**: プロトコルの細部を git に任せられる
- **Cons**: CGI ヘッダの解析、環境変数での設定が要り、失敗が CGI のステータス行に隠れる。上限や早期の拒否を制御しにくい
- **Why not**: 失敗をエラーコードで記録するという契約に合わない

## Consequences

### Positive
- 認可を起動前に行い、拒否のときは git を起動しない
- 失敗がサービスごとのエラーコード（`FG-GIT-*`）で記録される

### Negative
- 約 300 行のプロトコルの周辺処理を自前で持つ

### Risks
- 広告の形式の誤り → 実際の git クライアントでの E2E テスト（clone / fetch / shallow / partial / push）で検出する
