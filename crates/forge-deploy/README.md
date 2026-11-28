# forge-deploy

デプロイ（計画 14 C11、ADR-0031）。forge の worker から SSH で社内のアプリサーバーへ。環境（dev / stg / prod）・ターゲット・要求・承認の
ルートの側と、待ちのデプロイを 1 つずつ動かす worker の側を持つ。SQL は `forge_db::deploy`、判定は `forge_auth::authorize()`。
機能クレートで、他の機能クレートには依存しない（契約 9）。

| 公開の API | 中身 |
|---|---|
| `Deploy::new(db)` | `forge serve` が作る |
| `environments` / `deployments`（`DeployRead`） | ルートの proof の上で読む。ターゲットは指紋だけ（鍵は答えない） |
| `put_environment`（`DeployEnvironmentManage`）、`put_target`（`DeployTargetManage`） | 検査（`check_environment` / `check_target`）→ 変更のトランザクションで判定をやり直し、監査の行を同じトランザクションで書く |
| `request`（`DeployCreate`）、`approve`（`DeployApprove`、`DeploymentCtx`） | 要求は凍結期間を DB の `now()` で確かめる。承認はロックした行で判定をやり直す（要求者・MR の作成者は FG-DEPLOY-0009） |
| `run::run_once(db, transport, key_file, timeouts)` | worker の 1 回: 承認の無い要求を `blocked`、自動の要求、鍵の確認、1 つ取り出し、`execute` |
| `run::execute` | 全ターゲットに `deploy` → 全ターゲットに `health`。失敗したら `auto_rollback` の環境は変えたターゲットを `rollback` → `health`。host key の不一致はそれ以上どこにも触れない |
| `approval_facts` / `approval_summary`（`DeployRead`、M5 L5-8） | 承認の事実の一覧（コードが作る。範囲は `CommitSource::range` の git compare、分類は `forge_core::risk` の経路の規則）と、別にローカル LLM の要約（`summary::SummaryView`: なし・表示・機密はエージェントに伏せる、`StaleReason`）。承認は読まない。`summary::summary_input` が LLM の入力（`internal`） |
| `bundle`（計画 17 P17-7、ADR-0031 Amendment P17-7） | 種類 `pfs-ai-bundle` のリリース: `create_release`（`AiAssetReleaseCreate`。tag → 固定したコミット → 保護ブランチ・`forge/ai-eval` が success → 束のファイルの lint → `forge_agent::bundle` のマニフェストを agent の鍵で署名し確かめ直す）、`request_release`（段の順 影 → カナリア → 本番、承認の要る環境だけ）、`rollback_release`（直前の承認済みのリリースの要求）。git は `BundleSource`（`GitBundles`） |
| `transport::{DeployTransport, SshTransport, ssh_argv, classify}` | ssh は argv だけ（`-F none`、`BatchMode`、`ConnectTimeout`、固定した known_hosts だけ、`--` の後にホストと 3 語）。試験は trait の偽物 |

**エラーコード**: FG-DEPLOY-0001〜0014、リリースは FG-AIASSET-0008・0009（`errors.toml`）。**runbook**: `deploy`。**doctor**: `deploy.known_hosts`（forge-cli）。

**落とし穴**
- リモートの引数は ssh がつないでリモートのシェルに渡す。値はすべて `[A-Za-z0-9._-]` に限る（`forge_core::deploy` の検査と表の CHECK）。
- スクリプトの出力は保存も記録もしない（シークレットを含みうる）。stderr は ssh の失敗の分類にだけ使い、16 KiB を超えた分は読み捨てる。
- worker が途中で止まった `running` は再開しない（doctor が知らせる。runbook `deploy` の Mitigate 7）。
