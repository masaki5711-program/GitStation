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

## Amendment 2026-11-28（計画 17 P17-7 `feat/deploy-pfs-ai-bundle`: 種類 `pfs-ai-bundle` のリリース・3 つの段・直前の承認済みへの戻し）

計画 17 §1.8〜§1.11 の GitStation 側を入れた。利用者は不在で「推奨を選ぶ」と言っているので、計画に書かれていない点は次のとおり決めた。

1. **置き場所**: 新しいクレートは作らない。リリースの作成・段・戻しは `forge-deploy`（`bundle`）、SQL は `forge_db::deploy::ai_bundle`。
   マニフェスト・要約・署名は純粋な `forge_agent::bundle`（計画 16 A1 と同じ鍵 `agent.signing_keys_file`、同じ長さ付きの SHA-256 の符号化）。
   署名の文は **専用の領域** `"forge-pfs-ai-bundle-signature-v1\0"` ‖ 要約（要約の前置きも `"forge-pfs-ai-bundle-digest-v1\n"`）。部品の署名・
   エッジのマニフェストの署名は、同じ鍵・同じ 32 バイトでも束としては通らない（ADR-0041 §3 と同じ考え。テスト
   `a_part_signature_never_verifies_as_a_bundle`）。`forge-deploy` の依存に純粋な `forge-agent`・`forge-doc` と `toml` を足した（契約 9 の向きのまま、
   他の機能クレートには依存しない。`dependency_direction.rs`）。
2. **表 `forge.ai_bundle_releases`**（追記のマイグレーション `20270115110000`、`audited`、資源はプロジェクト）: 版・tag・**作成のときに tag が
   指したコミット**・tag の object・マニフェスト（JSON）・要約・kid・署名・ベンチのチェックの行の id・ファイル数・作った人。守り
   `ai_bundle_releases_guard` が UPDATE と DELETE を断る（XF001）。forge_app には SELECT と呼ぶ側の列の INSERT だけ。部品（ADR-0032 Amendment A3 §3）と
   同じく **プロジェクトへの外部キーを張らず、消去より長く残す**（PFS のローダーが入れた版を確かめ続けられる）。`UNIQUE (project_id, version)` と
   `UNIQUE (project_id, commit_sha)`: 版もコミットも 1 回だけ。デプロイの行は普通の `deployments`（その SHA）で、リリースとは SHA で結ぶ。
3. **tag の形** `assets/v<yyyy>.<ww>.<n>`（週は 01〜53、番号は 1〜999、先頭の 0 なし。表の CHECK も同じ）。hotfix も同じ形。
4. **作成の手順**（`POST …/ai-asset-releases`）: tag を git で引いて **その時のコミットを固定** → 既に出した版・コミットなら `already_released`
   （tag を動かしても効かない。テスト `moved_tag_has_no_effect`）→ 保護ブランチにあること（デプロイと同じ `CommitSource`）→ そのコミットの
   `forge/ai-eval` が `completed` かつ `success`（無ければ `bench_missing`、他は `bench_not_green`。**ベンチが緑でなければ何も署名しない**。テスト
   `bundle_signed_only_when_bench_green`）→ 束のディレクトリ（`skills`・`playbooks`・`agents`・`tools`・`models`・`bench`。`evals`・`thresholds`・
   `manifest` は入れない）のファイルを固定したコミットから読む（2000 ファイル・32 MiB まで、symlink・submodule は `not_a_regular_file`）→
   `gstn ai-asset lint` と同じ `forge_doc::ai_asset::lint` と `release_problems`（`lint_failed`）→ マニフェスト（ファイルごとの path・SHA-256・大きさ、
   `models/models.toml` の `name:tag@sha256`、ベンチの結論と要約 4 KiB まで）→ 鍵ファイルのいちばん大きい kid（登録済み・退役も失効もなし・同じ公開鍵。
   違えば FG-AGENT-0014）で署名 → 登録した公開鍵で **保存する JSON から** 確かめ直す → ロックしたプロジェクトで判定をやり直し、`ai_assets.enabled` を
   ロックの下で読み直して、行と監査の行 `ai_asset_release.created` を同じトランザクションで書く。全体に 60 秒の時間切れ。誤りは新しいコード
   **FG-AIASSET-0008** `BundleReleaseRefused`（info）。
5. **権限**: 新しい操作 `AiAssetReleaseCreate`（人だけ、Maintainer 以上、`api`）。部品の公開（A5）と違い **人のトークンも通す**（`gstn ai-asset release`）:
   リリースはどの環境も変えず、どの段も人の `DeployApprove`（Web のセッションだけ）が要るため。エージェントのトークン・ボット（`pfs-ai-bot`）・
   サービスアカウント・自動マージの主体・CI のジョブ・代理は断る。読み取りは `DeployRead`、段への要求と戻しは `DeployCreate`（新しい操作を作らない）。
   権限の表に allow / deny の行（`matrix_rows_ai_release.rs`、`matrix_rows_people.rs`）とテスト `prod_approve_denied_for_bot`。
6. **3 つの段 = 既存の 3 つの環境**: `dev` = `pfs-ai-shadow`、`stg` = `pfs-ai-canary`、`prod` = `pfs-ai-prod`（列も階層も足さない。API と gstn は
   段の名前でも層の名前でも受ける）。リリースの要求（`POST …/ai-asset-releases/{version}/deployments`）は同じトランザクションで: リリースが
   ある（`release_not_found`）、**環境が承認を要する**（`approval_required`。prod は ADR-0031 の CHECK のとおり常に）、**前の段で同じリリースが
   承認つきで成功している**（影 → カナリア → 本番、`stage_skipped` と `needs`）。誤りは新しいコード **FG-AIASSET-0009** `BundleDeployRefused`（info）。
   計画 §1.9 の「3 日・30 件・新旧の差を approver が見た」は PFS の指標（PFS-6・7）で、GitStation は数えない（承認する人が判断する）。
7. **リリースだけのプロジェクト**: `ai_assets.enabled` が真のプロジェクトでは、普通の SHA の要求（`POST …/deployments`）は `release_required`、
   自動のデプロイ（決定 9）は候補にしない。`false` に戻すと普通の要求に戻る（Maintainer の人だけ、監査つき）。既存のリリースの段への要求は
   設定によらず通る（計画 §0.4: 止まるのは新しいリリースだけ）。
8. **戻し**（`POST …/ai-asset-rollbacks {"environment"}`）: その環境で最後に成功した版のリリースより **古い**（リリースの順）リリースのうち、その環境で
   **承認つきで成功した** いちばん新しいものの、新しいデプロイの要求を作る（テスト `rollback_targets_last_approved`。2 回戻すとさらに古くなり、
   離れたばかりの版へは戻らない）。**戻しも承認が要る**（同じ SHA の再要求にも新しい承認、ADR-0029 の義務 6）。worker の失敗時の自動の戻し
   （`auto_rollback` の環境）は、リリースのデプロイなら **承認つきで成功した別のリリース** だけを前の版にする（承認なしの成功・普通の SHA へは戻さない）。
   計画 §8 の決定 2（本番でも直前の承認済みへは人の承認なしに戻す）は **PFS のローダー（PFS-5）が手元で行う runbook の手順**で、GitStation は
   リリースの一覧・確かめる材料・戻し先の要求の道を出す（GitStation の prod の環境は今までどおり自動の戻しを持てない）。
9. **gstn**: `gstn ai-asset release --project N --tag assets/v…`（作成）、`releases [--version v…]`（一覧・1 件とマニフェスト）、`release-deploy
   --version v… --environment pfs-ai-shadow|pfs-ai-canary|pfs-ai-prod`、`rollback --environment …`。承認は画面だけ（トークンは `session_required`）。

### P17-7 の独立レビュー（2026-11-28、deploy-safety-reviewer・security-reviewer）で変えた点

- （deploy HIGH、直した）worker の自動の戻し先が「承認つきで成功した別のリリース」だけで、**新しい方** にも戻りえた（v3 → v2 への戻しが失敗すると
  v3 へ「戻る」）。戻し先は **いま入れようとしているリリースより古い** リリースだけ（`approved_older_release_sql`。手の戻しと同じ順の規則）。
  テスト `a_failed_rollback_never_returns_to_the_newer_release`。
- （security HIGH、直した）束のファイルの path は `/` の区切りと `.`・`..` しか見ていなかった。ローダーがどの OS でもそのまま書ける形だけ
  （各区切りは ASCII の英数字と `.`・`_`・`-`、`.` で終わらない、255 バイトまで。`\`・`:`・制御文字・空白・非 ASCII を断る）、大文字・小文字だけが
  違う 2 つの path も断る（`canonical` と `parse_manifest` の両方）。
- （deploy MEDIUM、直した）**失効した鍵** で署名したリリースは段に要求できない（`signing_key_revoked`）し、手の戻し・worker の戻しの先にもならない。
- （deploy MEDIUM、直した）戻しの要求は、そのコミットが今も保護ブランチにあることを求めない（作成のときに確かめ、署名し、その段で承認された版。
  事故の最中にブランチが動いても最後の良い版へ戻れる）。承認は今までどおり要る。
- （security MEDIUM、直した）束のディレクトリの中で lint が知らないファイル（スクリプト・入れ子のファイル）は署名しない（`lint_failed`
  `rule=not_an_asset_file`。空のディレクトリの `.gitkeep` だけは許す）。
- （security LOW、直した）信頼できないマニフェストの大きさの和は桁あふれを確かめる（`too_large`）。署名の後の確かめは、**DB に入った jsonb の
  マニフェスト** でも同じトランザクションの中で行う（通らなければ何も残らない）。
- （security MEDIUM、受け入れ）作成は Maintainer の人 1 人で、人のトークンでもよい（上の 5）。署名した束は段の外でも確かめられるので、盗まれた
  トークンは緑のベンチのコミットに署名させうる。緩和: 段はすべて人のセッションでの承認、監査の行とログ `forge.deploy.bundle_released` に
  `credential_id`（トークンで作ったものを数えられる）。二人の目やセッションだけにするかは PFS-5 のローダーの信じ方と合わせて決める。
- （deploy LOW、受け入れ）`ai_assets.enabled` を切ると普通の SHA の要求に戻る（上の 7。Maintainer の人だけ、監査つき）。悪いと分かったリリースの
  「取り下げ」の印はまだ無い（戻し先から外すには、鍵の失効か新しい版）。

### 残すこと

- マニフェストに計画 §1.8 の「プレイブックの行」「元の MR と改善要求の一覧」はまだ入れていない（プレイブックはファイルの SHA-256 で覆う）。
  PFS-5（ローダー）の形が決まったら v2 のマニフェストで足す。
- `clippy.toml` の `disallowed-methods` に `forge_agent::bundle::sign_bundle` を足す変更は、エージェントモデルの設定の保護のフックが止めたため
  入れていない（人が足す。`read_signing_seed` の禁止は効いており、forge-deploy の 1 か所だけが `#[allow]` で呼ぶ）。
- 段の画面（リリースの一覧・承認の画面に版とマニフェストの要約）は無い（承認は既存のデプロイの承認の画面で行う）。
