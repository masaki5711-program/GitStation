# forge-client (`gstn`)

**Responsibility**: the client of REST API v1 — an HTTP client library and the
`gstn` binary (plan `01` §3.11). It runs on developers' PCs (Windows / Linux),
never on the server, and never touches the DB or server internals.

**Dependency decision** (ADR-0020): depends on `forge-errors` only (the
generated `FG-…` registry both sides must agree on). No server crate
(`forge-core` / `-db` / `-auth` / `-web` / `-hosting` / `-git`) and none of them
depends on this crate; `client_depends_only_on_forge_errors` checks it.

## 利用者向けの手引き（日本語）

### インストール

`gstn`（Windows は `gstn.exe`）を PATH の通った場所に置きます。インストーラーやランタイムは要りません。
配布（ポータルからのダウンロード、SHA-256 と署名、`gstn self-update`）は別の MR で足します。

### 設定

サーバーの URL を、次のどれかで指定します（後ろほど優先）。

1. 設定ファイル `config.toml`（Linux: `~/.config/gstn/`、Windows: `%APPDATA%\gstn\`）

   ```toml
   host = "https://git.example.internal"
   # ca_file = "C:\\certs\\internal-ca.pem"   # 社内 CA を足す（OS の証明書ストアに加えて使う）
   # timeout_secs = 30
   ```
2. 環境変数 `GSTN_HOST`（`GSTN_CA_FILE`、`GSTN_TIMEOUT`、`GSTN_CONFIG_DIR`）
3. オプション `--host https://…`

`http://` は、自分の PC（localhost）以外では使えません（トークンが平文で流れるため）。

### ログイン

トークンは、GitStation の画面で発行します（AI 用は `is_agent` のトークンを別に発行します）。

```
gstn auth login --with-token < token.txt     # 標準入力から読む。引数やプロンプトでは受け取らない
gstn auth status                              # ユーザー、スコープ、is_agent（トークン自体は出さない）
gstn auth logout                              # 保存したトークンを消す（サーバー側の失効はしない）
```

トークンは OS の資格情報の保管庫（Windows の資格情報マネージャー、Linux の Secret Service）に保存します。
使えない環境（画面のないサーバーなど）では、自分だけが読める（権限 0600）ファイル
`credentials.toml` に保存し、その旨を警告で表示します。トークンは画面にもログにも出しません。
CI などでは環境変数 `GSTN_TOKEN` でも渡せます。

### git の設定

```
gstn auth git-credential --install
git clone https://git.example.internal/team/app.git     # 素の git が自動でトークンを使う
```

`--install` は `git config --global credential.<host>.helper` に登録します。このホストにだけ答え、
ほかのホストには何も答えません。

### AI（Claude Code）からの使い方

- すべてのコマンドに `--json`。失敗時は終了コードと、stderr の JSON
  `{error_code, user_message, request_id, retryable, detail}` で判断できます。
  終了コード: 0 成功 / 1 その他 / 2 使い方の誤り / 3 認証・権限 / 4 見つからない / 5 衝突 / 6 一時的（再試行してよい）。
- `gstn help --json`（全コマンド・引数・例・必要なスコープ）、`gstn schema <コマンド>`。
- 専用のコマンドがない操作は `gstn api <METHOD> <path>`（`--field k=v`、`--field k:=<json>`、
  `--input file`、`--paginate`、`--dry-run`）。例: `gstn api GET /api/v1/projects --paginate --fields id,name`
- エラーコードは `forge explain FG-…` か `docs/errors/` で原因と対処を引けます（クライアント側は `FG-CLI-*`）。

### ホスティングのコマンド

```
gstn project list [--limit N] [--cursor C | --all]      # view / create / edit / delete --yes / restore
gstn group list|view|create [--parent-id N]|edit
gstn member list|add|set-role|remove --yes  (--project ID | --group ID)
gstn protected-ref list|add|remove --yes --project ID
gstn namespace resolve team/app                          # パスから project / group / user の id を引く
```

id は正の整数。破壊的な操作（`project delete` / `member remove` / `protected-ref remove`）は
`--yes` が必須で、無いときは何も送らずに終了コード 2 で終わる（`gstn` は対話で確認しない）。
`--all` は `next_cursor`（member は `next_after`）を、件数・サイズ・ページ数の上限つきで辿る。

## 開発者向けメモ

- コマンドを足すときは `src/cli.rs`（clap）と `src/registry.rs`（`help --json` の元）の両方に書く。
  `registry_and_clap_describe_the_same_commands` が食い違いを止める。
  `help --json` を変えたら `GSTN_UPDATE_SNAPSHOT=1 cargo nextest run -p forge-client` でスナップショットを更新し、差分を確認する。
- **C-1 のゲート**: サーバーに REST のルートが増えると `every_api_route_is_covered_or_planned` が落ちる。
  `src/coverage.rs` に、対応するコマンド（`Command`）か、予定（`Planned`）の行を足す。
- 構成: `config.rs`（設定の層）/ `store.rs`（トークンの保管、0600 のフォールバック）/ `http.rs`
  （タイムアウト、リダイレクトなし、ページ送り）/ `credential.rs`（git の資格情報ヘルパー）/
  `app.rs`（実行と出力）/ `coverage.rs`（C-1）。入出力は注入できるので、テストはプロセス内で stdout / stderr を検査する。
- 自己更新の足場: `config.toml` の `[update]` 表と `state.toml` を予約済み（今は無視）。
- OS の保管庫: Linux は `dbus-secret-service-keyring-store`（libdbus を静的に同梱。zbus 経由は
  RUSTSEC-2026-0372 などで `cargo deny` を通らないため使わない）、Windows は `windows-native-keyring-store`。
  Windows 側はこの MR では実機で検証していない（Linux 上でビルド・テストした）。

## Error codes

`FG-CLI-0001`…`0007` (usage, not logged in, server unreachable, response invalid, config invalid,
credential store failed, output failed). Server errors keep their own code and request id.
