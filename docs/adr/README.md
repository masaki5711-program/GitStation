# Architecture Decision Records

設計判断の記録。新しい ADR は `template.md` をもとに、番号を 1 つ進めて作る。

| ADR | Title | Status | Date |
|-----|-------|--------|------|
| [0001](0001-process-model-events-and-crate-layout.md) | プロセス構成、ドメインイベント、クレートの依存の向き | accepted | 2026-10-26 |
| [0002](0002-postgresql-in-development.md) | 開発環境も PostgreSQL にする（SQLite は使わない） | accepted | 2026-10-26 |
| [0003](0003-llm-verification-as-advisory-checks.md) | ローカル LLM による検証は Checks API の「助言」チェックとして出す | accepted | 2026-10-26 |
| [0004](0004-repository-kinds-and-spec-driven-development.md) | リポジトリの種類（spec / system）と仕様駆動開発 | accepted | 2026-10-26 |
| [0005](0005-kanflow-integration-event-feed-and-act-as.md) | kanflow 連携 — 汎用の作業項目プロバイダ、イベントの履歴 API、代理実行 | accepted | 2026-10-26 |
| [0006](0006-merge-safety-and-pipeline-diagnosis.md) | main を壊さない仕組みと、機械が読めるパイプラインの診断 | accepted | 2026-10-26 |
