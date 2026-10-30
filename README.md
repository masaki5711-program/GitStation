# GitStation — 社内 Git プラットフォーム

社内で完結する、GitHub / GitLab に相当する開発基盤です。目的は **AI によるシステム開発の加速**。
Git ホスティング、MR とレビュー、CI/CD と社内アプリサーバーへの自動デプロイ、ローカル LLM による
マージ・パイプラインの検証、kanflow（プロジェクト管理ツール）との連携、仕様駆動開発を提供します。
**実行環境は社内のサーバー**で、外部のサービスには依存しません。

コード上の識別子（クレート名 `forge-*`、コマンド `forge`、エラーコードの `FG-`）は開発時の仮称のままです。

エージェントモデル（Claude Code の設定・フック・ゲート）は 1 つ上の階層 `D:\app\git` にあり、
Claude Code はそこで起動します。

## 構成

| パス | 内容 |
|---|---|
| `crates/forge-errors` | エラー台帳 `errors.toml` と、そこから生成するエラー型・カタログ |
| `crates/forge-core` | 共通の基盤（ID の型、`RefName`、問い合わせ番号 `RequestId`、ログ設定） |
| `crates/forge-auth` | 認可の単一判定点 `authorize()` と権限マトリクス |
| `crates/forge-web` | HTTP 層（ルートの登録の仕組み、問い合わせ番号、エラーの表示、`forge serve`） |
| `crates/forge-git` | git の実行（唯一のラッパー）、バージョンの確認、リポジトリの保存先 |
| `crates/forge-cli` | `forge` コマンド（`explain`、`errors list`。M1 以降 `serve`、`doctor`、`diag`） |
| `docs/errors/` | エラーカタログ（**自動生成**。手で編集しない） |
| `docs/adr/` | 設計判断の記録 |
| `docs/brand/` | ロゴとアイコン（UI ではこのファイルを使う） |
| `ops/runbooks/` | 障害対応手順（エラーコードと紐づく） |
| `ops/incidents/` | 障害記録とポストモーテム、障害の記憶 `records.jsonl` |

## よく使うコマンド（このディレクトリで実行）

```
cargo build --workspace
cargo nextest run --workspace        # なければ cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p forge-errors --bin gen-catalog   # errors.toml を変えたら
cargo run -p forge-cli -- explain FG-SYS-0001
```

## ロードマップ

M1 Git の核 → M2 Web UI の基礎 → **M3 MR とレビュー（R1 パイロット）** → M4 CI/CD →
**M5 ローカル LLM（R2 正式運用）** → M6 Issue とナレッジ → **M7 文書ポータル（R3 全社展開）** → M8 拡張。

計画書（エージェントモデルのリポジトリ `../docs/plan/`）: `01` 全体計画、`02` kanflow 連携と外部 API、
`03` 仕様駆動開発、`04` CI/CD・コンフリクト・パイプラインの検証。設計判断は `docs/adr/`。
