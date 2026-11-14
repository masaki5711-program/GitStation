# Runbook: deploy（FG-DEPLOY-0001〜0013。関連: FG-AUTH-0002、FG-AUTH-0003、FG-WEB-0006、FG-REPO-0005）

M4 C11（計画 14 §4.1・§4.2、ADR-0031）のデプロイが期待どおりに動かないとき。forge の worker（タスク `deploy_runner`）が待ちの
デプロイを 1 つずつ取り、ターゲットごとに `ssh … -- <host> <script> deploy|health|rollback <sha>` を動かす。表は
`forge.deploy_environments`（環境: dev / stg / prod）・`forge.deploy_targets`（ホスト・固定した host key・スクリプトの名前）・
`forge.deployments`（要求・承認・状態・前の版）。ルートは `GET|PUT /api/v1/projects/{id}/deploy/environments[/{tier}[/targets/{name}]]`、
`GET|POST /api/v1/projects/{id}/deployments`、`POST …/deployments/{deployment_id}/approve`。doctor `deploy.known_hosts`。

**prod は人の承認が必須**。承認はどの環境でも Maintainer 以上の人の Web のセッションだけで、要求者と、その SHA の背後の人（MR の作成者・push した人・コミットの author と committer）は承認できない。prod は自動でロールバックしない。

## 前提（ターゲットのホストの準備）

- forge のホストにだけ置く秘密鍵 `deploy.ssh_key_file`（forge の利用者が所有、`chmod 600`）。公開鍵をターゲットの利用者の
  `~/.ssh/authorized_keys` に **`restrict,command="/opt/forge-deploy/dispatch"`** 付きで登録する（`dispatch` は
  `SSH_ORIGINAL_COMMAND` の 3 語 `<script> <verb> <sha>` を検証し、登録したスクリプトだけを動かす）。これが無いと、鍵が漏れたとき
  そのホストで何でもできる。
- スクリプトは `deploy <sha>`（その SHA を自分で取得して入れ替える）、`health <sha>`（0 で健全）、`rollback <sha>`（前の版に戻す）
  を受ける。出力は forge に残らない（シークレットを含みうる）。ホスト側のログに書く。
- **ロックの契約**: `deploy` と `rollback` は同じ排他ロック（例: `flock /var/lock/forge-deploy-<script>`）を取る。`rollback` はロックを
  待ってから戻す。forge が `deploy` を時間切れで止めてもリモートで動き続けることがあり、ロックが無いと後の `rollback` と競合する（ADR-0031 追記 6）。
- スクリプトの名前にシェル・インタプリタ（`sh`・`bash`・`python…` など）は使えない。dispatch は名前を 1 つの固定のスクリプトに対応づける。
- デプロイできるのは保護されたブランチにあるコミットだけ（prod はその push のパイプラインの成功も要る。FG-DEPLOY-0011）。
- ターゲットの host key は、ホストの管理者から別の経路で受け取る（`ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` の指紋と照合）。
  同じネットワークの `ssh-keyscan` の結果だけを信じない。

## Trigger

- worker のログに `forge.deploy.target_failed`（error、**alert**）: FG-DEPLOY-0003（接続できない）または FG-DEPLOY-0004（host key の不一致）
- worker のログに `forge.deploy.rollback_failed`（error、**alert**、FG-DEPLOY-0006）: ロールバックも失敗した。環境が壊れたままの可能性
- worker の監視役のログに `task=deploy_runner` の失敗（**alert**、FG-DEPLOY-0008）: 鍵のファイルが無い・読めない・権限が広い
- `forge.deploy.target_failed`（warn）: FG-DEPLOY-0007（スクリプトの失敗）・FG-DEPLOY-0005（ヘルスチェックの失敗）
- `forge.deploy.blocked`（info）: FG-DEPLOY-0001（承認が無い・切れた）・FG-DEPLOY-0002（凍結期間）
- `forge doctor` の `deploy.known_hosts` が FG-DEPLOY-0008 / FG-DEPLOY-0004（`stored_pin_invalid`）、または
  「running for more than 120 minutes」の警告
- 利用者から「デプロイが承認待ちのまま」「承認できない（FG-DEPLOY-0009 / FG-AUTH-0002）」と問い合わせがあった

## Impact

- FG-DEPLOY-0004 は **なりすましの可能性**。その環境へのデプロイは止まる。セキュリティの事象として扱う（SEV2）。
- FG-DEPLOY-0006 は環境が中途半端な状態（stg なら SEV3、prod の手動のロールバックの失敗なら SEV2）。
- FG-DEPLOY-0003 / 0008 はその環境（0008 はすべての環境）のデプロイが止まる。アプリ自体は前の版で動き続ける（SEV3）。
- FG-DEPLOY-0005 / 0007: stg は自動で前の版に戻る（`rolled_back`）。prod は `failed` のまま人が判断する（下の Mitigate 5）。
- forge の他の機能（push・CI・マージ）には影響しない。

## Diagnose（読み取りのみ）

1. デプロイの状態（`deployment_id` はログ・問い合わせ番号から）:
   `SELECT id, project_id, tier, sha, status, requested_by, merge_request_author, approved_by, approved_at, approval_expires_at, previous_sha, error_code, failed_target, created_at, started_at, finished_at FROM forge.deployments WHERE id = <deployment_id>;`
2. 環境の設定: `SELECT tier, requires_approval, approval_ttl_minutes, auto_rollback, auto_deploy_ref, freeze_windows, freeze_windows @> now() AS frozen FROM forge.deploy_environments WHERE project_id = <project_id>;`
3. ターゲットと固定した指紋: `forge doctor`（`deploy.known_hosts`: `project/tier/target host:port SHA256:…`）、または
   `SELECT name, host, port, login_user, host_key_type, host_key_fingerprint, script, enabled FROM forge.deploy_targets WHERE project_id = <project_id>;`
4. ログ（`request_id` / `deployment_id` で絞る）: `forge.deploy.requested` → `forge.deploy.approved` → `forge.deploy.target_failed`
   （`target`・`host`・`step`・`reason`・`exit_code`・`pinned_fingerprint`）→ `forge.deploy.rollback_failed` → `forge.deploy.finished`。
5. 接続の確認（forge のホストから、読み取りだけ。`true` だけを送る。`command=` の制限があれば dispatch が拒否するのが正しい）:
   `ssh -F none -i <deploy.ssh_key_file> -o BatchMode=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile=<指紋を確かめた known_hosts> -p <port> -l <user> -- <host> true`
6. host key の不一致（FG-DEPLOY-0004）: ホストの管理者にホスト上で `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` を実行してもらい、
   doctor の指紋と比べる。再インストール・鍵の更新の予定があったかを確かめる。
7. 承認できない: FG-DEPLOY-0009（`reason=requester` / `merge_request_author`）は別の Maintainer が承認する。FG-AUTH-0002
   `reason=session_required` は prod をトークン（`gstn`）で承認しようとした（Web の画面で承認する）。エージェント・サービスアカウント・
   自動マージの主体は承認できない（FG-AUTH-0003、仕様）。
8. `running` のまま残る（doctor の警告）: worker が途中で止まった。ホスト側でどこまで進んだかをスクリプトのログで確かめる。

9. **止まったデプロイ（FG-DEPLOY-0012）**: worker の心拍が 5 分途絶えた `running` は次の回で `failed` になる（ロールバックはしない）。
   `SELECT id, sha, previous_sha, started_at, heartbeat_at FROM forge.deployments WHERE id = <deployment_id>;` と worker のログ
   （`forge.deploy.finish_failed`、異常終了）を見る。各ターゲットで動いている版をスクリプトのログで確かめる。
10. **接続先が許されない（FG-DEPLOY-0013）**: ログの `reason`（`denied_address` / `outside_allowed_networks` / `forge_port`）と `host`。
   ホストの DNS が 127.0.0.1・169.254.x.x などを返していないか、`deploy.allowed_networks` に入っているかを確かめる。

## Mitigate（変更は人の承認のあと）

1. **鍵（FG-DEPLOY-0008）** `[MUTATING — needs approval]`: 秘密鍵を `deploy.ssh_key_file` に置き、`chown <forge の利用者>`、
   `chmod 600`、`forge worker` を再起動。戻し: 設定を元に戻して再起動（デプロイは止まったまま）。
2. **接続（FG-DEPLOY-0003）** `[MUTATING — needs approval]`: ネットワーク・DNS・sshd・`authorized_keys` を直す。その後
   `POST /api/v1/projects/{id}/deployments` で同じ SHA をもう一度要求する（prod は新しい承認が要る）。
3. **host key の不一致（FG-DEPLOY-0004）** `[MUTATING — needs approval]`: 正当な変更と確認できたときだけ、管理者が
   `PUT /api/v1/projects/{id}/deploy/environments/{tier}/targets/{name}` で新しい `host_key_type` / `host_key` を登録する（監査に残る）。
   確認できなければ **登録し直さず**、インシデントとして `/incident` を開く。戻し: 前の鍵を登録し直す。
4. **凍結（FG-DEPLOY-0002）・承認切れ（FG-DEPLOY-0001）**: 仕様どおり。凍結期間が誤りなら Maintainer が環境の `freeze_windows` を直す
   `[MUTATING — needs approval]`。承認の期間が短すぎるなら `approval_ttl_minutes` を延ばす。その後もう一度要求する。
5. **prod のヘルスチェックの失敗（FG-DEPLOY-0005）**: 人が決める。前に戻すなら `previous_sha` を `POST …/deployments` で要求し、
   別の Maintainer が承認する `[MUTATING — needs approval]`。前へ直すなら修正の SHA を同じ手順で出す。
6. **ロールバックの失敗（FG-DEPLOY-0006）** `[MUTATING — needs approval]`: ターゲットのリリースのディレクトリから手で前の版に戻す
   （スクリプトの手順書に従う）。その後、確かな SHA を要求してデプロイの履歴をそろえる。
7. **`running` が残った**（心拍の掃除が動かないとき。通常は Diagnose 9 のとおり自動で `failed` になる） `[MUTATING — needs approval]`: ホストの状態を確かめた上で、DB の管理者の接続（forge_owner）で
   `UPDATE forge.deployments SET status = 'failed', error_code = 'FG-DEPLOY-0007', failed_target = '<name>', finished_at = now(), updated_at = now() WHERE id = <deployment_id> AND status = 'running';`
   （その環境の次のデプロイが動けるようになる）。戻し: なし（終わった行は変わらない。もう一度要求する）。

8. **止まったデプロイの後** `[MUTATING — needs approval]`: ターゲットをスクリプトのログで確かめ、必要なら手で前の版に戻し、確かな SHA を要求する。
9. **接続先** `[MUTATING — needs approval]`: DNS を直すか、管理者がターゲットの `host` を直す（監査に残る）。正当なネットワークなら
   `deploy.allowed_networks` に足して `forge worker` を再起動。戻し: 設定を元に戻して再起動。

## Verify

- `SELECT status, error_code FROM forge.deployments WHERE id = <新しい deployment_id>;` が `succeeded`。
- `forge doctor` の `deploy.known_hosts` が ok（指紋の一覧、警告なし）。
- ターゲットのアプリのヘルスチェックが健全（スクリプトの `health` と同じ確認）。

## Escalate

- FG-DEPLOY-0004 で正当な変更と確認できない: セキュリティの事象。`/incident`（SEV2）、ホストの管理者と情報セキュリティの担当へ。
- prod の FG-DEPLOY-0006: アプリの担当者と、そのアプリのオンコールへ。
- 鍵が漏れた疑い: 鍵を作り直し、すべてのターゲットの `authorized_keys` を差し替える（ADR-0013 の回転と同じ）。

## 知っておくこと

- 実行のときにも凍結期間と承認を確かめる（要求のときに通っても、実行のときに凍結なら `blocked`）。自動化のための抜け道は無い。
- 承認は 1 つのデプロイ（1 つの SHA）だけに効く。同じ SHA をもう一度要求すると新しい承認が要る（ADR-0029 の義務 6）。
- `auto_deploy_ref` の環境（prod 以外）は、その branch の push のパイプラインが成功した最新の SHA を `system` の要求で作る。
- 成功したデプロイの SHA の CI の成果物は `keep` になり、30 日で消えない。

## 関連

- ADR-0031、ADR-0029（義務 6）、ADR-0013（秘密のファイル）
- runbook `ci-secrets`（CI の変数）、`ci-pipelines`
