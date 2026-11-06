# Runbook: m3-test-environment（M3 の試験環境）

計画 11 §7 の試験環境を、このホストで立ち上げ・使い・片づける手順です。**合成データだけ**を使い、本番の Kanflow（`:3000`）や
ほかのサービスのコンテナ・ボリューム・ネットワークには触れません。

## 構成とポート

すべて `127.0.0.1` だけで待ち受けます。docker compose のプロジェクト名は **`gs-m3-test`**（このプロジェクトのものだけを作り・消す）。

| 部品 | ポート | 中身 |
|---|---|---|
| GitStation（`forge serve`、release ビルド、UI 埋め込み） | 127.0.0.1:**28080** | ホストのプロセス（`forge worker` も同じ設定で動く） |
| `gs-test-pg` | 127.0.0.1:**25432** | `timescale/timescaledb:2.28.1-pg16`。GitStation の DB `forge`（nextest の 55432 とは別） |
| Keycloak `forge-m3-test` | 127.0.0.1:**28180** | `quay.io/keycloak/keycloak:26.4.2`、`start`（本番モード）、`KC_DB=postgres` |
| `kc-test-pg` | （ホストへは公開しない） | Keycloak の DB。compose のネットワーク内で Keycloak だけが使う |
| mailpit（SMTP） | 127.0.0.1:**28025** | 通知（N1）の受け口。`notify.smtp_tls = none` |
| mailpit（画面） | 127.0.0.1:**28026** | 受信したメールを見る |

使ってはいけない: **25434**（Kanflow の試験環境）、3000・3100・3200・8080・15432〜15434・15442・15445・18025・18080・55432・58080（ほかが使用中）。
起動前に `netstat -ano` で空きを確かめ、使用中ならスクリプトが止まります。

## 秘密の置き場と安全策

- 生成した秘密は `ops/test-env/.local/` だけにある（gitignore 済み）。Windows では `up` が `icacls` で現在のユーザーだけに権限を絞る（継承を外す）。
- `down` / `reset` は、PID ファイルの PID が**この環境の `forge`（このリポジトリの `target/release/forge`、設定は `.local/forge.toml`）**であることを
  確かめてから止める。違う場合は何も止めず、古い PID ファイルだけを消す（共有ホストで PID が再利用されても他のプロセスを止めない）。
- エラー文に出る生成済みのパスワードは `***` に伏せる。
- journey は `composeProject` が `gs-m3-test` でなければ動かず、DB の書き換えは試験の利用者（Keycloak の `sub`）のセッションだけに限る。

## 前提

- Docker（Rancher Desktop）、Node 22 以上、`git`。リポジトリのルートで作業する。
- GitStation を release でビルド済みであること（UI を埋め込む）:
  `cd web && npm ci --ignore-scripts && npm run build && cd .. && cargo build --release -p forge-cli --features embed-ui`
  （別の場所のバイナリは環境変数 `FORGE_BIN` で指す）。

## 立ち上げ（bring up）と種まき（seed）

```
node ops/test-env/test-env.mjs up
```

1 回で次をすべて行う（何度実行してもよい）:

1. 秘密を `ops/test-env/.local/`（**gitignore 済み**、コミットしない）に生成する: DB のパスワード、Keycloak の管理者と client の秘密、
   `cursor-keys` / `audit-seal-keys` / `web-session-keys`、`forge.toml`、`forge-migrate.toml`、`journey.env.json`。
2. `docker compose -p gs-m3-test` で `gs-test-pg`・`kc-test-pg`・mailpit・Keycloak を起動する。
3. **種まき（DB）**: `ops/db/bootstrap/01〜03` を `gs-test-pg` の superuser で実行し、`forge_app` / `forge_migrator` のパスワードを設定し、
   `forge db migrate` を適用する（`db-bootstrap.md` の開発機の手順と同じ）。
4. **種まき（Keycloak）**: realm `forge-m3-test` を `ops/test-env/keycloak/realm-forge-m3-test.json` から起動時に取り込む（config-as-code）。
   - client: `gitstation`（redirect `http://127.0.0.1:28080/-/auth/callback`）、`kanflow-test`（`http://127.0.0.1:3300/*`）。秘密は取り込み時に環境変数から入る（ファイルには無い）。
   - ロール: 各 client の `access`。ID トークンの `resource_access.<client>.roles` に出る。
   - mapper: `employee_number`（社員番号）。管理者だけが編集できる属性。
   - 合成の利用者（パスワードは全員 `synthetic-password`、社員番号 `SYN1001`〜）: `alice`、`bob`、`admin`、`dora`（無効化の試験の相手）は `access` あり、`carol` は `access` なし。
5. `forge serve` と `forge worker` をホストのプロセスとして起動する（ログ: `.local/serve.log`、`.local/worker.log`）。`/readyz` が通るまで待つ。

### 設計の判断

- **Keycloak は `start`、HTTP を明示して許可**: この試験ホストには TLS の終端がない。`KC_HOSTNAME=http://127.0.0.1:28180`、`KC_HTTP_ENABLED=true`。
  GitStation はループバックの `http://` の issuer だけを受け付ける（ADR-0021 §13）ので `auth.oidc.ca_file` は不要。
  実際の社内環境は `https` と社内 CA（`ca_file`）を使う。**この構成は試験専用**。
- **バックチャネルのログアウトは試験環境では使わない**: Keycloak（コンテナ）からホストの 127.0.0.1 の GitStation へは届かない。
  無効化は ADR-0021 §7 のリフレッシュ（5 分ごと）で効く。試験は `refresh_checked_at` を進めて待たずに確かめる。
- GitStation は compose の外（ホスト）で動かす: 計画 11 §7 の「release でビルドして `forge serve` + worker」のとおり。

## 確かめる（本物の Keycloak での通し）

```
GS_M3_TESTENV=ops/test-env/.local/journey.env.json npm --prefix web run journey:keycloak
```

`keycloak_disable_takes_effect_on_next_request`: ログイン → ログアウト → 再ログイン → （統制: 確認の期限が来ても有効な利用者は続く）
→ Keycloak で `dora` を無効化 → 次の要求が 401 → 再ログインも Keycloak が拒否。終わりに `dora` を有効に戻す。
`GS_M3_TESTENV` がなければ**スキップ**（終了コード 0）なので、環境のない CI は赤くならない（`npm run journey` にも含まれる）。
失敗の画像は `SHOTS=<dir>` で保存できる。

- 画面: http://127.0.0.1:28080/ （`alice` / `synthetic-password` でログイン。ログインは Keycloak の画面に移る）。
- Keycloak の管理画面: http://127.0.0.1:28180/ 、利用者 `synthetic-admin`、パスワードは `ops/test-env/.local/compose.env` の `KC_ADMIN_PASSWORD`。
- メール: http://127.0.0.1:28026/

## 状態の確認

```
node ops/test-env/test-env.mjs status
```

## 片づける（tear down）

```
node ops/test-env/test-env.mjs down     # forge を止め、gs-m3-test のコンテナを消す（データのボリュームは残る）
node ops/test-env/test-env.mjs reset    # 上に加え、gs-m3-test のボリュームと .local（秘密・設定）を消す
```

どちらも `docker compose -p gs-m3-test` の範囲だけを対象にする。**ほかのプロジェクトのコンテナ・ボリュームには触れない**
（`docker system prune`、`docker volume prune`、プロジェクト名を付けない `down -v` は禁止）。

## 困ったとき

| 症状 | 見るところ |
|---|---|
| `up` が `port(s) already in use` で止まる | 使用中のポートを持つプロセスを確かめる。他のサービスなら止めずに、`ops/test-env/compose.yml` のポートを近くの空きに変えてこの表も直す |
| Docker の `timed out dialing Hyper-V socket` | 一時的。スクリプトは 2 回まで再試行する。続くなら Rancher Desktop を確かめる |
| ログインで Keycloak に届かない | `docker logs gs-m3-test-keycloak-1`、`forge doctor`（`--config ops/test-env/.local/forge.toml`）、`keycloak-unreachable.md` |
| ログインしたが 403 / FG-AUTH | `carol`（`access` なし）か社員番号の不一致。`oidc-login.md`、`oidc-identity-link.md` |
| 画面が殻のままで UI がない | `--features embed-ui` を付けずにビルドした。`web` をビルドしてから再ビルド |

## Verify

- `node ops/test-env/test-env.mjs status` の 5 つのポートがすべて `listening`、`forge serve` / `forge worker` が `running`。
- 上の journey が `ok`。
- `docker ps` に `gs-m3-test-*` 以外の変更がない（ほかのコンテナの稼働時間が変わっていない）。

## Escalate

- 本番の Kanflow・ほかのサービスに影響が出た疑いがあれば、直ちに止めて利用者に知らせる。このランブックの手順は、それらに触れないよう作ってある。
