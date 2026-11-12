# Runbook: ci-secrets（FG-CI-0016、FG-CI-0017、FG-CI-0018、FG-CI-0021。関連: FG-SYS-0002、FG-WEB-0006、FG-CI-0008）

M4 C6（計画 14 §4.1・§9 の 2、ADR-0013、ADR-0029 の決定 8 と C6 の追記）の CI の変数（シークレット）が期待どおりに動かないとき。
表 `forge.ci_variables`（値は `ci.secret_keys_file` の鍵で封じた暗号文だけ。監査はプロジェクトに `ci_variable.created` /
`.updated` / `.deleted`）、ルート `GET|PUT|DELETE /api/v1/projects/{id}/ci/variables[/{name}]`（`CiVariableManage`: Maintainer 以上、
人だけ）、`gstn ci variable list|set|delete`、Runner の取得（`POST /api/v1/runner/jobs/request`）で渡す `secrets`、ログの保存の前の
マスク、doctor `ci.secret_keys`。

値は **保護されたブランチへの push のパイプライン**（作成のときに保護されていた: `ci_pipelines.source = 'push'` かつ `protected`）の
ジョブにだけ、そのジョブが `secrets:` に挙げた名前だけを渡す。マージリクエスト・手動・定期のパイプラインには決して渡さない。

## Trigger

- `forge serve` のログに `forge.ci.secret_key_unavailable`（error、**alert**、FG-CI-0016）: シークレットを求めたジョブの値を開けず、
  ジョブを失敗にした（Runner には渡していない）。`reason` は `not_configured` / `key_missing`（`key_id`）/ `value_unreadable`
- 変数の書き込み（`PUT …/ci/variables/{name}`、`gstn ci variable set`）が 503 FG-CI-0016、またはログの追記（Runner の
  `POST …/log`）が 503 FG-CI-0016（マスクする値を開けないのでチャンクを保存しない。Runner は 1 回 warn してそのチャンクを捨てる）
- `forge doctor` の `ci.secret_keys` が FG-CI-0016（`keys_unreadable` / `not_configured` / `key_missing`）
- `forge serve` が起動しない: FG-SYS-0002 `config_key=ci.secret_keys_file`（鍵のファイルが読めない・形が違う・権限が広い）
- `forge serve` のログに `forge.ci.secret_on_unprotected_ref`（info、FG-CI-0017）または `forge.ci.secret_not_defined`
  （info、FG-CI-0018）: ジョブを失敗にした。利用者から「デプロイのジョブがすぐ失敗する」と問い合わせがあった
- Runner のホストのログに FG-CI-0008 `reason=secrets_mismatch` / `secret_invalid`: 渡された名前が `.forge-ci.yml` と違う、
  値が 1 行でない（サーバーの不具合。報告する）

## Impact

- FG-CI-0016 は**シークレットを使うジョブがすべて止まる**（デプロイを含む）。シークレットの無いジョブ・push・閲覧・マージの判定は
  影響を受けない。保護ブランチのデプロイが止まるなら SEV3、それ以外は SEV4。
- FG-CI-0017 / 0018 はそのジョブだけ（定義か変数の登録の誤り）。SEV4 以下。
- 値は Runner のホストとジョブのコンテナの環境にしか出ない。ログはマスクするが、ジョブのスクリプトが値を加工して出すと残りうる
  （マスクは安全網で、境界は ADR-0029 の「出力は印を継ぐ」）。

## Diagnose（読み取りのみ）

1. 鍵のファイル（値も鍵も出さない）: `forge doctor` の `ci.secret_keys`。
   - `keys_unreadable`（`config_reason`）: `not_absolute` / `insecure_permissions`（所有者だけが読める 0600 でない）/ `invalid_key` /
     `invalid_key_id` / `duplicate` / `empty` / `unreadable`。`ls -l <ci.secret_keys_file>`（中身は表示しない）
   - `not_configured`: `ci.secret_keys_file`（`FORGE_CI_SECRET_KEYS_FILE`）が設定されていないのに変数がある
   - `key_missing`（`key_id`、`variables`）: 変数を封じた鍵がファイルから消えた
2. 鍵ごとの変数の数（読み取り用の接続は暗号文を読めない）:
   `SELECT key_id, count(*) FROM forge.ci_variables GROUP BY key_id;`
3. 失敗にしたジョブ:
   `SELECT j.id, j.name, j.status, j.error_code, j.secrets, p.source, p.protected, p.ref FROM forge.ci_jobs j JOIN forge.ci_pipelines p ON p.id = j.pipeline_id WHERE j.id = <job_id>;`
   - `error_code = FG-CI-0017`: `source` が `push` でない、または `protected = false`。計画（FG-CI-0001 `secret_on_unprotected_ref`）が
     先に止めるはずなので、古い forge が書いた行か不具合
   - `error_code = FG-CI-0018`: ログの `missing` の名前がプロジェクトに無い（`gstn ci variable list --project <id>`）
4. 誰がいつ変えたか（値は監査にも無い）:
   `SELECT occurred_at, action, actor_id, details FROM forge.audit_log WHERE resource_type = 'project' AND resource_id = <project_id> AND action LIKE 'ci_variable.%' ORDER BY id DESC LIMIT 20;`
5. 1 つのジョブの経過: ログの `job_id` で `forge.ci.job_claimed`（`secrets_released` = 渡した数）→ `forge.ci.job_completed`、
   または `forge.ci.secret_*` を追う（`request_id`）。

## Mitigate（変更は人の承認のあと）

- **鍵のファイルを戻す**（運用者、承認が要る）: 秘密の保管場所から `ci.secret_keys_file` を戻す（1 行に `<key_id> <64 桁の 16 進>`、
  `forge` の利用者だけが読める 0600、絶対パス）。`forge serve` を再起動し、失敗したパイプラインをやり直す:
  ```
  gstn pipeline retry --project <id> <pipeline_id>
  ```
  **古い値を封じた鍵を失ったときに新しい鍵を作っても、その値は開けない**。新しい鍵で始め、変数を入れ直す（下）。
- **鍵の入れ替え**（運用者、承認が要る）: 新しい鍵を大きい `key_id` で**足す**（古い鍵は残す）→ `forge serve` を再起動。新しい値は新しい鍵で
  封じる。既存の値を新しい鍵にするには各変数を設定し直す。`SELECT key_id, count(*) …`（Diagnose 2）で古い `key_id` が 0 になってから古い鍵を消す。
- **変数を設定し直す**（Maintainer、人）: 値は標準入力から（引数・履歴に残さない）:
  ```
  gstn ci variable set --project <id> DEPLOY_TOKEN < token.txt
  gstn ci variable list --project <id>
  ```
  マスクしない値は `--unmasked`。マスクする値は 8 バイト以上。値は 1 行（改行・制御文字・両端の空白・先頭の引用符は不可、400 FG-WEB-0006
  `param=value` と `reason`）。
- **FG-CI-0017**（定義の担当）: シークレットを使うジョブは保護ブランチへの push だけで動くようにする（`rules: { branches: [main],
  merge_requests: false }`、ブランチを保護する）。
- **FG-CI-0018**（Maintainer）: 変数を設定するか、ジョブの `secrets:` から名前を外して、パイプラインをやり直す。
- **値の漏えい**（Maintainer、すぐに）: 値を発行した側（デプロイ先・外部のサービス）で失効させ、新しい値を `gstn ci variable set` で入れる。
  ログに残った値は消さない（ログは追記だけ。マスクの漏れなら不具合として報告し、ログの保持の 90 日を待つか運用の手順で消す、承認が要る）。

## Verify

- `forge doctor` の `ci.secret_keys` が ok（`every key listed`）。
- シークレットを使うジョブをやり直し、ログの `forge.ci.job_claimed` の `secrets_released` が名前の数と同じで、ジョブのログに値が無い
  （`[masked]` になっている）。

## シークレットを受け取ったジョブの成果物（FG-CI-0021）

値を渡したジョブ（`forge.ci_jobs.secrets_released = true`）は、取得のときに渡した値の写し（`forge.ci_job_secret_snapshots`）を持つ。
その成果物は:

- 送られてくる間に、渡したマスクする値の形（値、base64 — 前に何かある場合も —、16 進、パーセント、JSON のエスケープ）を探し、
  見つかれば保存しない（422 FG-CI-0021。ジョブは FG-CI-0020 `server_code=FG-CI-0021` で失敗）。値はログにも応答にも出さない。
- 保存できたものも、読めるのはプロジェクトの Maintainer 以上の人だけ。AI（エージェントのトークン・サービスアカウント・
  `Forge-Context-Audience: agent`）は FG-AUTH-0022 `reason=secret_job`。`needs` で受け取れるのは保護された push のパイプラインの中だけ。

Diagnose: `SELECT id, name, secrets, secrets_released FROM forge.ci_jobs WHERE id = <job_id>;`、
`SELECT name, attempt, masked, created_at FROM forge.ci_job_secret_snapshots WHERE job_id = <job_id>;`（暗号文は読まない）、
`forge serve` のログの FG-CI-0021（`job_id`・`path`）。

Mitigate: 成果物にするパスにシークレットを書かない（`env > out.txt` をしない。値は /tmp など成果物にしない場所で使う）。
**加工した値（暗号化・圧縮）は見つからない**: それでも Maintainer しか読めないが、漏れた疑いがあれば値を発行元で失効させ、
`gstn ci variable set` で入れ替える（ジョブの途中で変えても、そのジョブには取得のときの値の写しでマスクが続く）。

## 知っておくこと

- `protected_only` は既定で真。値を渡すのは**常に**保護されたブランチへの push のパイプラインだけ（ADR-0029 の義務 5）。
  偽にしても届く範囲は広がらない。後の ADR で広げるときに移行が要らないように列を残した。
- ジョブのトークン（`fgj_…`）と同じく、値は Runner の取得の応答で 1 回だけ渡る（`no-store`）。Runner は値をジョブの
  ディレクトリの外のファイル（`<work_dir>/job/secrets/job.env`、0600）に書き、`podman create --env-file` に渡してすぐ消す。
  argv・ログ・`Debug` には出さない。
- エージェント（AI のトークン）・サービスアカウント・代理・Runner・ジョブのトークンは、変数の一覧・設定・削除のどれも断られる
  （`CiVariableManage` は人だけ）。

## 関連

- ADR-0029（CI の主体）・ADR-0013（秘密のファイル）、計画 14 §4.1・§4.2・§5 の C6
- runbook `ci-runner`（Runner のホスト）、`ci-pipelines`、`config-invalid`（FG-SYS-0002）
