# ADR-0020: クライアント `gstn` のクレートの置き方と、トークンの保管

**Date**: 2026-11-01
**Status**: accepted
**Deciders**: ユーザー（M1 phase 8b の計画で `gstn` を別の MR の系列にすることを承認）、Claude

## Context

`gstn`（`01` §3.11）は開発者の PC で動くクライアントで、REST API v1 だけを呼ぶ。CLAUDE.md §3.9 の依存の向き
（`forge-web / forge-cli → … → forge-core → forge-errors`）はサーバー側の層の話で、クライアントをどこに置くかは決まっていない。
また、トークンを OS の保管庫に置くとき、Linux の Secret Service への経路によっては `cargo deny` が通らない。

## Decision

1. 新しいクレート `forge-client`（ライブラリ + バイナリ `gstn`）を置く。**依存してよいサーバー側のクレートは `forge-errors` だけ**
   （`FG-…` の台帳とメッセージを、サーバーと同じ元から使うため）。`forge-core`・`forge-db`・`forge-auth`・`forge-web`・
   `forge-hosting`・`forge-git` には依存しない。サーバー側のどのクレートも `forge-client` に依存しない。
   `client_depends_only_on_forge_errors` が検査する。
2. クライアント側の失敗は新しい領域 `CLI`（`FG-CLI-0001`〜`0007`）に登録する。`http_status` は分類の目安（クライアントでは HTTP を返さない）。
   終了コードは、サーバーのコードの登録上の分類（なければ HTTP ステータス）から決める。
3. トークンは OS の保管庫（Windows: 資格情報マネージャー、Linux: Secret Service）に置き、使えなければ権限 0600 のファイル
   （Windows ではユーザーのプロファイル下）に置いて警告を出す。0600 でないファイルは読まない。Linux は libdbus を静的に同梱する
   `dbus-secret-service-keyring-store` を使い、zbus 系（RUSTSEC-2026-0372 の `zvariant` などが未保守）は使わない。
4. トークンの送信先は設定したホストだけ。リダイレクトは辿らない。`http://` は localhost のみ（`allow_http` で明示したときを除く）。

## Alternatives Considered

### `forge-core` の型（`Scope`、`RequestId`）を共有する
- **Pros**: 型が 1 つ。
- **Cons**: `forge-core` は sqlx 向けの依存と AES などを持ち、クライアントの配布物が大きくなる。サーバー内部の型がクライアントの API になる。
- **Why not**: クライアントは API v1 の JSON だけを契約にする。スコープは文字列で十分。

### zbus 経由の Secret Service（pure Rust）
- **Why not**: `cargo deny`（unmaintained の助言）が落ちる。助言を `deny.toml` で許す理由がない。

## Consequences

- クライアントは単一の実行ファイルにできる（libdbus を同梱、外部サービスなし）。C の `cc` が要る（ビルド時のみ）。
- 新しい領域 `CLI` は、エージェントモデルの `validate-error-registry.js` の `AREAS` と `forge-error-contract.md` にも加える必要がある
  （この MR の引き継ぎ事項）。
- `self-update`（チェックサム + 署名）、配布、Windows 実機での保管庫の検証は後続。設定は `[update]` 表と `state.toml` を予約している。
