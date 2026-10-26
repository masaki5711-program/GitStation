# forge — 社内 Git プラットフォーム（仮称）

社内で完結する Git プラットフォームの製品リポジトリです。エージェントモデル
（Claude Code の設定・フック・ゲート）は 1 つ上の階層 `D:\app\git` にあり、
Claude Code はそこで起動します。

## 構成

| パス | 内容 |
|---|---|
| `crates/forge-errors` | エラー台帳 `errors.toml` と、そこから生成するエラー型・カタログ |
| `crates/forge-core` | 共通の基盤（問い合わせ番号 `RequestId`、ログ設定） |
| `crates/forge-cli` | `forge` コマンド（`explain`、`errors list`。M1 以降 `serve`、`doctor`、`diag`） |
| `docs/errors/` | エラーカタログ（**自動生成**。手で編集しない） |
| `docs/adr/` | 設計判断の記録 |
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

M1 Git の核（HTTP での clone / push、ユーザー、権限、doctor、diag）→ M2 Web UI →
M3 SSH・Webhook・バックアップ → M4 CI/CD と自動デプロイ → M5 Issue / MR →
M6 Wiki → M7 文書ポータル。詳細は `../docs/plan/00-agent-model-plan.md`。
