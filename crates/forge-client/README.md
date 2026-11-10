# forge-client (`gstn`)

**Responsibility**: the client of REST API v1 — an HTTP client library and the
`gstn` binary (plan `01` §3.11). It runs on developers' PCs (Windows / Linux),
never on the server, and never touches the DB or server internals.

**Dependency decision** (ADR-0020 and its Amendment, ADR-0026 §8): depends on
`forge-errors` (the generated `FG-…` registry both sides must agree on) and the
pure `forge-doc` (the document lint and format `gstn doc` shares with
`forge/doc-lint`). No server crate (`forge-core` / `-db` / `-auth` / `-web` /
`-hosting` / `-git`) and none of them depends on this crate;
`client_depends_only_on_forge_errors_and_forge_doc` checks it.

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

サービスアカウントの管理（M3 S0、管理者。トークンは `admin` スコープ）:

```
gstn admin service-account list|view <id>|tokens <id>
gstn admin service-account update <id> [--display-name X] [--purpose X] [--owner <user id>]
gstn admin service-account revoke-token <id> <token id> --yes   # 漏えいのときはすぐに
```

作成とトークンの発行は管理者の Web セッションだけ（トークンからは FG-AUTH-0002）。手順は Runbook `service-accounts`。

連携（M3 I1a）。管理者の一覧・詳細・結びつけ（トークンは `admin` スコープ）と、プロジェクトの結びつけ（Maintainer）:

```
gstn admin integration list|view <id>|bindings <id>
gstn integration-binding list --project <id>
gstn integration-binding set --project <id> --integration <id> --external-project <kanflow の id>
gstn integration-binding remove --project <id> --integration <id> --yes
```

連携の登録と変更は管理者の Web セッションだけ（トークンからは FG-AUTH-0002）。手順は Runbook `integrations`。

id は正の整数。破壊的な操作（`project delete` / `member remove` / `protected-ref remove`）は
`--yes` が必須で、無いときは何も送らずに終了コード 2 で終わる（`gstn` は対話で確認しない）。
`--all` は `next_cursor`（member は `next_after`、mr は `next_before`）を、件数・サイズ・ページ数の上限つきで辿る。

### マージリクエストのコマンド（M3 R1a）

```
gstn mr create --project 7 --source feature/login --target main --title 'ログイン画面' [--description …] [--draft]
gstn mr list --project 7 [--state open|closed|merged|all] [--limit N] [--before IID | --all]   # 新しい順。既定は open
gstn mr view --project 7 --iid 3
gstn mr edit --project 7 --iid 3 [--title …] [--description …] [--target BRANCH] [--draft | --ready]
gstn mr close --project 7 --iid 3
gstn mr reopen --project 7 --iid 3
gstn mr versions --project 7 --iid 3                      # 版の一覧（M3 R2）
gstn mr diff --project 7 --iid 3 --version 2 [--patch]     # 版の差分（merge-base..head）
gstn mr diff --project 7 --iid 3 --from 1 --to 2 [--patch] # 版どうしの比較（head..head）
gstn mr mergeability --project 7 --iid 3                  # マージできるか（最後の確認の結果。確認は始めない）
gstn mr merge --project 7 --iid 3 --sha <head> [--method squash] [--delete-source-branch] [--wait]  # マージ（M3 R4。トークンは merge_execute）
gstn mr merge-status --project 7 --iid 3 [--wait]         # 最新のマージの試行（queued〜applied / superseded / failed）
gstn mr approve --project 7 --iid 3 --sha <head>          # 見た head を承認（M3 R5。トークンは merge_approve。数えるかを返す）
gstn mr unapprove --project 7 --iid 3                     # 自分の承認の取り消し
gstn mr approvals --project 7 --iid 3                     # 承認の評価（数えない理由、ルール、CODEOWNERS、止める理由）
gstn approval-rule set --project 7 --name backend --required 2 [--target-pattern refs/heads/main] [--user U] [--group G]  # 承認のルール（Maintainer）
gstn approval-rule list --project 7 / gstn approval-rule remove --project 7 --name backend --yes
gstn mr status --project 7 --iid 3 --json                 # マージできるか・止める理由のすべて（M3 R6。安定した契約 docs/api/merge-status.md）
gstn merge-settings show --project 7                      # マージの設定（方式・既定・ソースの削除・承認の引き継ぎ・コミッターの承認・CODEOWNERS）
gstn merge-settings set --project 7 [--allow M]... [--default M] [--reset-approvals-on-push BOOL] …   # 設定の変更（Maintainer。指定しない値はそのまま）
gstn codeowners-enforcement suspend --project 7 --for 60  # CODEOWNERS の強制を期限つきで止める（承認された手順。runbook codeowners）
gstn codeowners-enforcement enforce --project 7           # 強制に戻す
gstn merge-rule set --project 7 --rule-id 2 [--approvals N] [--check ci/gates]... [--resolved-discussions] [--no-direct-push]  # 保護ブランチのマージの条件
gstn merge-rule remove --project 7 --rule-id 2 --yes
gstn mr discussions --project 7 --iid 3                   # スレッドとコメントの一覧（M3 R3。古くなった行は [outdated]）
gstn mr comment --project 7 --iid 3 --body '…'            # 全体へのコメント（新しいスレッド）
gstn mr comment --project 7 --iid 3 --version 2 --path src/lib.rs --line 10 [--side old|new] --body '…'  # 行へのコメント
gstn mr comment --project 7 --iid 3 --discussion 12 --body '…'   # 返信
gstn mr resolve --project 7 --iid 3 --discussion 12 [--unresolve] # スレッドの解決（人だけ。エージェントのトークンは拒否）
```

`--iid` はプロジェクトの中の番号（`!3`）。ブランチ名・タイトル・説明は JSON の本文で送り、パスには入れない。
同じ source / target の開いている MR が既にあると FG-REVIEW-0001（終了コード 5）。

### 文書のコマンド（計画 13 段 2。手元だけ）

```
gstn doc lint [<パス>…] [--json] [--migration-warnings]   # 文書を型と他の文書に照らして検査（forge/doc-lint と同じ検査）
gstn doc fmt [--check] [<パス>…] [--json]                  # 文書を整形（1 文 1 行、項目と属性の順）。--check は書かずに確かめる
```

- **サーバーには何も送りません**。ログインも設定も要りません。今いるフォルダから上へたどって見つけた git のリポジトリの、
  作業ツリーのファイル（`git ls-files` の一覧。`.gitignore` で除いたものは読まない）と `.forge/doctypes/*.yml` を読みます。
- 文書は、前付けに `doctype` を書いた `.md`（どのフォルダでも）。それ以外の Markdown は読み飛ばします（`fmt` も書き換えない）。
- `lint` は参照を確かめるためにリポジトリの文書をすべて読み、パスを渡したときはその下のファイルの問題だけを出します。
  誤りがあると終了コード 1（`FG-DOC-0001`。型の定義 `.forge/doctypes/<型>.yml` が使えないときは `FG-DOC-0003`）。
- 壊れた参照（`ref_broken`、まだ GitStation に無い文書への参照）は **誤り** です。移行の間は `--migration-warnings` で警告に下げられます
  （手元だけ。MR の `forge/doc-lint` の答えが正）。サーバーの `forge/doc-lint` で下げるのは、次のリポジトリの設定です。

### 移行中の警告の設定（計画 13 Q13。サーバーに送る）

```
gstn doc migration show  --project <id> [--json]                 # 期限・今それが効いているか・サーバーの日付（UTC）と決められる最後の日
gstn doc migration set   --project <id> --until YYYY-MM-DD       # 期限まで壊れた参照を警告に下げる（Maintainer 以上、人だけ）
gstn doc migration clear --project <id> --yes                    # 今すぐ終える（壊れた参照は誤りに戻る）
```

- 期限は **必須** で、サーバーの「今日」（DB の日付、UTC）より後、180 日以内。外れると `FG-DOC-0005`、日付の形でないと `FG-WEB-0006`。
  手元の PC の時計は使いません。
- 期限の日の終わり（UTC）まで警告で、翌日からは何もしなくても誤りに戻ります。下げている間、MR の `forge/doc-lint` の要約の 1 行目に
  `migrating: until YYYY-MM-DD` と出ます。
- `doc migration` は `doc fmt` / `doc lint` と違い、ログインと設定（トークン）が要ります。
- `fmt --check` は何も書かず、変わるファイルがあるか、確かめられないファイル（名前を挙げた文書でないファイル、大きすぎる・UTF-8 でない文書）があると終了コード 1（`FG-DOC-0004`）。読んだ後に誰かが書き換えた文書は上書きしません（`FG-CLI-0007`）。`--check` が無いと書き換えます（一時ファイルに書いてから置き換えるので、
  途中で止まっても文書が半分になることはありません）。型の定義が使えない文書はそのまま残し、終了コード 1（`FG-DOC-0003`）。
- 上限（`.md` 5,000 個、合計 64 MiB など）に当たると、問題が無くても終了コード 1（`FG-DOC-0002`）。黙って合格にはしません。

## 開発者向けメモ

- コマンドを足すときは `src/cli.rs`（clap）と `src/registry.rs`（`help --json` の元）の両方に書く。
  `registry_and_clap_describe_the_same_commands` が食い違いを止める。
  `help --json` を変えたら `GSTN_UPDATE_SNAPSHOT=1 cargo nextest run -p forge-client` でスナップショットを更新し、差分を確認する。
- **C-1 のゲート**: サーバーに REST のルートが増えると `every_api_route_is_covered_or_planned` が落ちる。
  `src/coverage.rs` に、対応するコマンド（`Command`）か、予定（`Planned`）の行を足す。
  ルートを呼ばないコマンド（`doc fmt` / `doc lint` など手元だけのもの）は `LOCAL_COMMANDS` に理由と一緒に書く
  （`commands_without_a_route_are_local` が確かめる）。
- 構成: `config.rs`（設定の層）/ `store.rs`（トークンの保管、0600 のフォールバック）/ `http.rs`
  （タイムアウト、リダイレクトなし、ページ送り）/ `credential.rs`（git の資格情報ヘルパー）/
  `app.rs`（実行と出力）/ `coverage.rs`（C-1）。入出力は注入できるので、テストはプロセス内で stdout / stderr を検査する。
- 自己更新の足場: `config.toml` の `[update]` 表と `state.toml` を予約済み（今は無視）。
- OS の保管庫: Linux は `dbus-secret-service-keyring-store`（libdbus を静的に同梱。zbus 経由は
  RUSTSEC-2026-0372 などで `cargo deny` を通らないため使わない）、Windows は `windows-native-keyring-store`。
  Windows 側はこの MR では実機で検証していない（Linux 上でビルド・テストした）。

## Error codes

`FG-CLI-0001`…`0009` (usage, not logged in, server unreachable, response invalid, config invalid,
credential store failed, output failed, wait timed out, input failed). `gstn doc`: `FG-DOC-0001`…`0004`
(lint problem, bounded, type definition invalid, format would change). Server errors keep their own code and request id.
