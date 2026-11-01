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
| [0007](0007-drive-upload-pack-receive-pack-directly.md) | Smart HTTP は upload-pack / receive-pack を直接起動する | accepted | 2026-10-26 |
| [0008](0008-protected-refs-pre-receive-with-policy-snapshot.md) | 保護ブランチの判定 — 事前の検査と pre-receive フック | accepted | 2026-10-26 |
| [0009](0009-storage-paths-from-repository-id.md) | リポジトリの保存先はリポジトリ ID だけから決める | accepted | 2026-10-26 |
| [0010](0010-authz-model-and-route-registry.md) | 認可のモデルとルートの登録の仕組み | accepted | 2026-10-26 |
| [0011](0011-test-database-strategy.md) | テスト用の DB — docker compose の PostgreSQL とテストごとの新しい DB | accepted | 2026-10-26 |
| [0012](0012-outbox-gap-free-sequencer.md) | Outbox のイベントに欠番のない順序番号を付ける | accepted | 2026-10-26 |
| [0013](0013-configuration-layers-and-secret-files.md) | 設定の層（ファイル → 環境変数 → フラグ）と、秘密情報をファイルで渡すこと | accepted | 2026-10-26 |
| [0014](0014-migration-policy.md) | マイグレーションの方針 — 明示的に適用し、起動時に検査し、前に向かってだけ直す | accepted | 2026-10-26 |
| [0015](0015-url-and-namespace-scheme.md) | URL と名前空間の設計 — GitHub の語彙を `/-/` の後ろに置き、入れ子のグループと両立させる | accepted | 2026-10-27 |
| [0016](0016-domain-events-outbox-and-audit-log.md) | ドメインイベントの Outbox、カーソル方式のイベントの履歴、監査ログ | accepted | 2026-10-27 |
| [0017](0017-local-ai-autofix-loop.md) | ローカル LLM による MR の自動修正ループと、kanflow からの承認・マージ | accepted | 2026-10-28 |
| [0018](0018-db-roles-append-only-and-audit-sealing.md) | DB のロール、追記のみの強制、監査ログの封印 | accepted | 2026-10-27 |
| [0019](0019-personal-access-tokens-and-authentication.md) | 個人用アクセストークンと認証、無効化のガード | accepted | 2026-10-29 |
| [0020](0020-gstn-client-crate-and-credential-storage.md) | クライアント `gstn` のクレートの置き方と、トークンの保管 | accepted | 2026-11-01 |
| [0021](0021-oidc-keycloak-login-and-web-sessions.md) | OIDC（Keycloak）のログインと Web のセッション | accepted | 2026-11-01 |
| [0022](0022-web-ui-foundation-react-typescript-embedded.md) | Web の画面の土台（React + TypeScript、埋め込みの配信） | accepted | 2026-11-01 |
