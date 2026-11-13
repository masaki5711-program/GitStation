# ADR-0031: デプロイ — forge の worker から SSH、固定した host key、prod は人の承認

**Date**: 2026-11-13
**Status**: proposed（accepted は C11 `feat/deploy-ssh` の `deploy-safety-reviewer`・`security-reviewer`・`authz-matrix-auditor` のレビューで CRITICAL / HIGH が無いとき）
**Deciders**: Masaki Fujikawa（ユーザー、2026-11-10「推奨で進めて」: 計画 14 §9 の決定 8、04 §8「stg は自動ロールバック・prod は人の判断」）、Claude

## Context

計画 14 C11（04 §5、01 §3.4）は社内のアプリサーバーへのデプロイを入れる。デプロイは本番を変える（CLAUDE.md 契約 7）。壊れたときに、誰が何を承認し、どのホストで何が動き、なぜ止まったかを、ログと表だけで言えることが要る。
制約: オンプレだけ（外部のサービスに頼らない）、git と同じく外部のプログラムは argv で動かす（契約 5 の考え方）、鍵は DB とログに出さない（ADR-0013）、
自動化（エージェント・サービスアカウント・自動マージ）は prod を承認できない（計画 14 §7）、ジョブの再実行は承認の代わりにならない（ADR-0029 の義務 6）。

## Decision

1. **新しい機能クレート `forge-deploy`**。forge の worker（タスク `deploy_runner`）が待ちのデプロイを `FOR UPDATE SKIP LOCKED` で 1 つずつ取り、SSH で実行する。Runner（ADR-0029）は使わない（デプロイの鍵を Runner のホストに置かない）。SQL は `forge_db::deploy`、ルートは forge-web。
2. **表は 3 つ**（追記のマイグレーション `20261214100000`）。`deploy_environments`（プロジェクトごとに `dev` / `stg` / `prod`、承認の要否、承認の有効期間、凍結期間 `tstzmultirange`、自動ロールバック、自動デプロイの ref）と `deploy_targets`（ホスト・ポート・ユーザー・固定した host key・スクリプトの名前）は監査つき。`deployments`（SHA・要求者・承認者・状態・前の版）は派生の状態で、要求と承認は同じトランザクションで監査の行を書く。DB の CHECK で **prod は常に承認が要り、自動ロールバックと自動デプロイを持たない**。
3. **SSH は argv だけ**: `ssh -F none -i <鍵> -o BatchMode=yes -o ConnectTimeout=<秒> -o StrictHostKeyChecking=yes -o UserKnownHostsFile=<一時ファイル> -o GlobalKnownHostsFile=none -o HostKeyAlgorithms=<固定の型> -o UpdateHostKeys=no -o IdentitiesOnly=yes -o IdentityAgent=none -o ForwardAgent=no -o ClearAllForwardings=yes … -p <port> -l <user> -- <host> <script> <verb> <sha>`。環境変数は PATH だけ、stdin は閉じ、全体に時間切れ。リモートで動くのは **ターゲットに登録したスクリプトの名前だけ**で、引数は `deploy|health|rollback` と 16 進の SHA に限る（どの値も `[A-Za-z0-9._-]` だけなので、リモートのシェルが解釈する文字が無い）。ターゲット側は `authorized_keys` の `restrict,command=` で固定することを runbook で求める。
4. **host key は固定**: ターゲットに公開鍵（型と base64）を登録し、指紋 `SHA256:…` を表示・doctor・ログに出す。実行のたびにその 1 行だけの known_hosts を作る。不一致（`Host key verification failed` など）は **FG-DEPLOY-0004（alert）で止まり、やり直さず、新しい鍵を受け入れる道は無い**。直すのは管理者がターゲットの鍵を登録し直すこと（`DeployTargetManage`、監査）。
5. **鍵は `deploy.ssh_key_file`**（ADR-0013 の秘密のファイル: 絶対パス、通常のファイル、Unix では 0600）。forge は中身を読まず、ssh に `-i` で渡すだけ。無い・権限が緩いときは FG-DEPLOY-0008（alert）で、デプロイは始まらない。
6. **承認**（`DeployApprove`、authorize() の 1 か所）: 承認の要る環境では、人（エージェントのトークン・サービスアカウント・ボット・代理・Runner・Job は不可）の Maintainer 以上。**prod は Web のセッションだけ**（トークンは `session_required`）、**要求者と、その SHA をマージした MR の作成者は承認できない**（FG-DEPLOY-0009、管理者も）。自動マージの主体は人の種類を持たないので、`people_only` で断られる。承認は 1 つのデプロイ（1 つの SHA）に結びつき、`approval_ttl_minutes`（既定 60）で切れる。同じ SHA をもう一度要求しても新しい承認が要る（ADR-0029 の義務 6）。
7. **凍結期間は要求のときと実行のときの両方**で DB の `now()` で判定する。実行のときに凍結中なら `blocked`（FG-DEPLOY-0002）で終わり、自動化のための抜け道は無い。承認が実行のときに切れていても `blocked`（FG-DEPLOY-0001）。
8. **確認とロールバック**: 全ターゲットに `deploy` → `health`。失敗したとき、環境の `auto_rollback`（stg の既定は真、prod は持てない）なら、取り出したときに記録した**前の版**（その環境で最後に成功した SHA）へ `rollback` → `health`。ロールバックの失敗・前の版が無いときは FG-DEPLOY-0006（alert）。prod の失敗は人が runbook `deploy` で判断する。worker が途中で止まった `running` は再開しない（doctor が知らせ、人が判断する）。
9. **stg への自動デプロイ**: `auto_deploy_ref` を持つ環境（prod 以外）は、その ref の push のパイプラインが成功した最新の SHA を `system` の要求で作る（承認の要る環境なら承認を待つ）。

## Alternatives Considered

### Runner のジョブとしてデプロイする（`.forge-ci.yml` の deploy のジョブ）
- **Pros**: 仕組みが 1 つで済む。
- **Cons**: デプロイの鍵を Runner のホストとジョブに渡すことになり、ジョブのスクリプト（AI が書きうる）が鍵に触れる。再実行が承認の代わりになりやすい（ADR-0029 の義務 6）。
- **Why not**: 鍵と承認の境界を forge の中に閉じたい。

### known_hosts を共有のファイルで持つ / `accept-new`
- **Cons**: 初回の接続で偽のホストを信じる。どのターゲットにどの鍵かが表から読めない。
- **Why not**: 固定と監査ができない。

### リモートでコマンドの文字列を組む（スクリプトの本文・任意の引数）
- **Why not**: リモートのシェルでの注入の面を作る。スクリプトの中身はターゲットの管理者が置く。

## Consequences

### Positive
- 誰が承認し何が動いたかが `deployments`・監査の行・ログ（`deployment_id`・`target`・`request_id`・`error_code`）で追える。
- ssh は試験では trait（`DeployTransport`）の向こうにあり、argv・host key の判断・状態の遷移を本物の ssh なしで試せる。

### Negative
- 成果物の転送（scp）はまだ無い。スクリプトは SHA を受け取り、自分で取得する（社内のレジストリ・forge からの fetch）。
- 2 人の承認（§9 の決定 8 の任意）はまだ無い。

### Risks
- ターゲットの `authorized_keys` に `command=` が無いと、鍵が漏れたときにそのホストで何でもできる → runbook `deploy` の前提に書き、鍵は forge のホストだけに置く。
- worker の停止で `running` が残る → doctor `deploy.known_hosts` が古い `running` を数え、runbook で人が決める。

## Amendment 2026-11-13（C11 のレビュー: deploy-safety-reviewer・security-reviewer）

1. **デプロイできるコミット**: そのプロジェクトの**保護されたブランチから到達できるコミット**だけ（保護規則に合うブランチの先端と `git merge-base`、forge-git の browse API、argv・時間切れ）。適用された MR のマージも保護ブランチへの直接の push も、どちらも保護を通っているので両方を認める（MR のマージに限ると、保護ブランチへの push を許した設定のプロジェクトがデプロイできない）。**prod はさらに、その SHA の push のパイプラインが成功していること**。外れたら FG-DEPLOY-0011（`commit_not_found` / `not_on_protected_branch` / `no_succeeded_pipeline`）。自動の要求（決定 9）は作成時に保護されていた push のパイプライン（`ci_pipelines.protected`）だけ。
2. **承認できない人**: 要求者に加え、その SHA の**背後の人すべて**（その SHA をマージ結果か head に持つ**すべての** MR の作成者、そのコミットを push した人（`push_commits`）、コミットの author と committer（メールでアカウントに対応））。承認者にはコミットの件名・author を API で見せる（`deployments.commit_*`、要求の時に git から読んで固定）。
3. **承認は全環境で Web のセッションだけ**（`DeployApprove` を session-only に。トークンは FG-AUTH-0002 `session_required`。`gstn` に承認のコマンドは無い）。承認の後にターゲットが変わったら実行しない（`blocked`、FG-DEPLOY-0001 `target_changed_after_approval`）。
4. **止まった worker**: 実行中は 30 秒ごとに `heartbeat_at` を書く。5 分途絶えた `running` は、どの worker の次の回（起動直後を含む）でも `failed`（FG-DEPLOY-0012、監査の行あり、ロールバックはしない）。終わりの記録に失敗したら 3 回やり直し、それでも駄目ならこの掃除に任せる。worker の遷移（開始・成功・失敗・ロールバック・ロールバックの失敗・blocked・abandoned）はすべて同じトランザクションで監査の行を書く。
5. **ssh の 255**: 接続が開かなかったことを ssh のメッセージが示すとき（refused / timed_out / dns / auth / network / handshake）だけ「未接続」とし、それ以外の 255 は**スクリプトが動いたかもしれない**（`connection_lost`）としてロールバックの対象に入れる。
6. **リモートのロック（ターゲットのスクリプトの契約）**: ssh を時間切れで止めてもリモートのスクリプトは動き続けうる（`-T` には端末が無く、SIGHUP が届かないことがある）。スクリプトは `deploy` と `rollback` で**同じ排他ロック**（例: `flock /var/lock/forge-deploy-<script>`）を取り、`rollback` はそのロックを待ってから前の版に戻す（forge の `rollback` の時間切れは `deploy.script_timeout_secs`）。こうして時間切れの後のロールバックが、まだ動いている `deploy` を追い越さない。
7. **接続先の制限**: ループバック・未指定・リンクローカル（169.254.169.254 を含む）・マルチキャストのアドレスと、forge 自身の HTTP のポートには接続しない。`deploy.allowed_networks`（CIDR）を設定するとその中だけ。登録時（アドレスの直書き）と**接続のたびに名前を引いた後**に確かめ、ssh は確かめたアドレスに `HostKeyAlias=<名前>` で接続する（確かめと接続の間の DNS の変化で行き先が変わらない）。外れたら FG-DEPLOY-0013。API の読み手にはコードだけ、詳しい理由はログ。
8. **細部**: スクリプトの名前にシェル・インタプリタ（sh・bash・python… など）は使えない。ssh の `HOME` は呼び出しごとの空のディレクトリ。ssh の失敗の分類（上の 5 の class）をログの `reason` に出す。
