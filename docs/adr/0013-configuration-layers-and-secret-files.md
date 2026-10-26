# ADR-0013: 設定の層（ファイル → 環境変数 → フラグ）と、秘密情報をファイルで渡すこと

**Date**: 2026-10-26
**Status**: accepted
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## Context

DB の接続先、プールの大きさ、各種タイムアウトなど、設定の項目が M1 phase 2 で一気に増える。それまでは clap の `env` 属性（`FORGE_BIND`、`FORGE_STORAGE_ROOT`）だけで、設定ファイルはなかった。計画 §4.2 は「ファイル + 環境変数。`forge doctor` で検証する」としている。

設定の誤りが既定値に置き換わって黙って動くことは、契約 §3-2（失敗を隠す代替手段の禁止）に反する。DB のパスワードがログ、設定ファイルの見本、プロセスの一覧に出ることも避けたい。

## Decision

- `forge-core::config` で型付きの設定を読み込む。層は優先度の低い順に、組み込みの既定値 → TOML ファイル（`--config` / `FORGE_CONFIG`）→ 環境変数 → コマンドラインのフラグ。
- 環境変数の名前はキーから決める（`database.max_connections` → `FORGE_DATABASE_MAX_CONNECTIONS`）。このため `FORGE_BIND` は `FORGE_SERVER_BIND` に改名した。
- 未知のキーは失敗にする。設定ファイルのつづりの誤りも、設定の名前空間（`FORGE_SERVER_` / `FORGE_STORAGE_` / `FORGE_DATABASE_`）の下にある未知の環境変数も、どちらも失敗にする。ほかの `FORGE_*`（git フックに渡す変数など）は無視する。
- 改名前の名前（`FORGE_BIND`）や空の値は、無視せずに失敗にする。
- 誤りはすべて読み込みの時点で FG-SYS-0002 とし、`config_key` と `reason` を付ける。値が 0 のタイムアウトは不可。マイグレーション中の statement タイムアウトも上限を持つ（既定 10 分。契約 §3-2）。
- パスワードは `database.password_file`（絶対パス）**だけ**で渡す。URL に書かれたパスワード（`user:pass@` も `?password=` / `passfile` / `sslpassword` も）は拒否する。Unix では、グループや他のユーザーが読めるパスワードファイルも拒否する。
- DB の URL は `postgres://user@host[:port]/dbname` だけとし、クエリはすべて拒否する（`query_in_url`）。理由は 3 つある。クエリで接続先や TLS を上書きできてしまうこと。sqlx が知らないパラメータを値ごとログに出すこと（`?pasword=` のような打ち間違いでパスワードが残る）。そして、ログに出る接続先と実際の接続先を一致させるため。
- 接続の設定は URL をドライバに渡さず、host / port / user / dbname を明示的に組み立てる。`~/.pgpass` は読まない。libpq と互換の環境変数（`PGPASSWORD`、`PGSSLROOTCERT`、`PGOPTIONS` など）が設定されていたら FG-SYS-0002 `libpq_env` にする。これらは `password_file` や TLS の設定をすり抜けるため。
- DB の TLS は `verify-full`（既定）、`require`、`disable` の 3 つだけにする。`verify-ca` は用意しない。sqlx（rustls）の信頼ストアには公開 CA のルートが入っていて、`tls_root_cert` はそこに追加されるだけだからである。ホスト名を検査しない `verify-ca` では、どの公開証明書でも通ってしまう。`require` と `disable` は、ループバック以外のホストで使うと警告をログに出す。
- **受け入れたリスク**: `verify-full` でも公開 CA のルートを信頼する（sqlx 0.8 では信頼ストアを社内 CA だけに絞れない）。そこで、DB のホスト名には公開の証明書が発行されない社内の名前（公開ドメインの下にない名前）を使う。sqlx で絞れるようになるか、TLS の設定を自前で組んだ時点で見直す。
- 本番のサーバーは Linux とする。Windows（開発機）ではパスワードファイルの ACL を検査しない。これは受け入れたリスクとする。
- 秘密情報の型は `Debug` でも `Display` でも中身を出さない。URL の表示は host / port / dbname だけにする。設定ファイルの構文エラーは位置（行・列）だけをログに出す。toml のエラー文は問題の行を引用するため付けない。

## Alternatives Considered

### figment / config クレート
- **Pros**: 層の合成と出どころの追跡が最初からある
- **Cons**: 依存が増える。誤りを FG-SYS-0002 の `config_key` / `reason` に対応させるための変換が要る
- **Why not**: キーは十数個で、自前の実装でも 400 行以内に収まる。誤りの形を契約どおりにそろえやすい

### 環境変数だけ（clap の `env`）
- **Pros**: 今のままで済む
- **Cons**: 未知のキーを検出できない。`forge doctor` で検証する対象のファイルがない
- **Why not**: 項目の数が増えると運用で間違えやすい

### パスワードを環境変数で直接渡す
- **Why not**: `/proc/<pid>/environ` や診断の出力に残りやすい。ファイルなら権限で守れる

## Consequences

### Positive
- 設定の誤りは起動前に、キーの名前と理由の付いた 1 件のエラーとして分かる
- 秘密情報が出力に入らないことを型で保証できる

### Negative
- `FORGE_BIND` を使っていた手順は `FORGE_SERVER_BIND` に変える必要がある（M1 はまだ本番で動かしていない）

### Risks
- 新しいキーを `KEYS` に足し忘れる → 環境変数の名前がキーから導かれていることをテストで確かめる。未知のキーは失敗になるので、足し忘れればすぐに分かる
