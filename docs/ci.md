# CI（GitHub Actions）

GitStation の CI は `.github/workflows/ci.yml`。`main` への push と、すべての
pull request で動く。GitHub はソースのホスティングだけに使い、GitStation 自体は
社内サーバーで動かす（この CI は製品の実行環境ではない）。

## ジョブ

| ジョブ（チェック名） | 内容 |
|---|---|
| `fmt + clippy` | `cargo fmt --all -- --check`、`forge-cli` の通常依存に `test-support` 機能が入っていないことの確認、`cargo clippy --workspace --all-targets -- -D warnings` |
| `tests (nextest + PostgreSQL 16)` | PostgreSQL 16 のサービスコンテナ（`ops/dev/compose.yml` と同じユーザー・パスワード・DB 名・ポート 55432、`max_connections=200`）に対して `cargo nextest run --workspace --profile ci -j 4 --no-fail-fast --retries 1`。JUnit（`target/nextest/ci/junit.xml`）を成果物 `nextest-junit` として保存し、再実行で通ったテスト（不安定なテスト）をジョブのサマリーと警告に出す |
| `generated artifacts up to date` | `cargo run -q -p forge-errors --bin gen-catalog -- --check`（`docs/errors/`）、`cargo run -q -p forge-web --bin gen-authz -- --check`（`docs/authz/`） |
| `cargo-deny + cargo-audit` | `cargo deny check`（`deny.toml`: 脆弱性、ライセンスの許可リスト、bans、取得元）と `cargo audit`（`.cargo/audit.toml` の免除を読む） |

- Rust のバージョンは `rust-toolchain.toml` の `channel` を読んで使う（ワークフローに
  バージョンを二重に書かない）。
- 同じブランチ / PR の新しい push は、PR では実行中の CI を取り消す。`main` の実行は取り消さない。
- DB テストはスーパーユーザー `forge_dev` で接続し、テストごとに DB を作り、
  `forge_*` ロールのパスワードを合成値に戻す。そのため接続先は必ずループバック
  （`127.0.0.1:55432`）にする。
- forge-git のテストは `git` を実行する（最低 2.38）。ubuntu-latest の git を使い、
  ログに `git version` を出す。

## 設定ファイル

- `.config/nextest.toml` — `ci` プロファイル（fail-fast なし、再実行 1 回、
  10 分で止まらないテストを打ち切る、JUnit 出力）。手元の実行は既定のプロファイルのまま。
- `deny.toml` — ライセンスの許可リストは 2026-10-31 時点の `Cargo.lock` の
  ライセンス（`cargo deny list -l license`）。新しいライセンスの依存が入ると CI が
  落ちるので、確認してから追加する。
- `.cargo/audit.toml` — cargo-audit の免除（RUSTSEC-2023-0071、理由はファイル内）。
  cargo-deny は実際に使う依存だけを見るので、同じ免除は不要。

## 不安定なテスト

再実行で通ったテストは隔離しない。サマリーに出たテストごとに Kanflow に PBI を起票する
（`docs/plan/07-implementation.md` §1）。

## ブランチ保護で必須にする

GitHub のリポジトリ設定 → Branches（または Rules → Rulesets）で `main` に規則を作り、
「Require status checks to pass before merging」を有効にして、次の 4 つを必須にする
（一度 CI が動いた後でないと候補に出ない）:

- `fmt + clippy`
- `tests (nextest + PostgreSQL 16)`
- `generated artifacts up to date`
- `cargo-deny + cargo-audit`

あわせて「Require branches to be up to date before merging」と、force-push の禁止
（`main` は force-push しない）を有効にする。

## まだ入っていないもの（計画 07 §1）

- エージェントモデルのゲート（`validate-authz-coverage.js`、`validate-error-registry.js`）。
  GitStation_Agent は非公開リポジトリなので、読み取り用のトークン（シークレット）を
  決めてから追加する。
- 失敗したジョブを Kanflow の `report_failure` に送る処理（サービスアカウントが必要）。
