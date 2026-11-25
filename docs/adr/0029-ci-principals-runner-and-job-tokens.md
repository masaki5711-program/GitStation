# ADR-0029: CI の主体 — Runner と Job の資格情報、CI の信頼の境界

**Date**: 2026-11-10
**Status**: proposed（計画 14 §3.2 の決定を ADR にした。accepted は C0 `feat/auth-ci-principals` の `security-reviewer`・`authz-matrix-auditor` のレビューで CRITICAL / HIGH が無いとき）
**Deciders**: Masaki Fujikawa（ユーザー、2026-11-10「推奨で進めて」）、Claude

## Context

M4 の CI（計画 14）では、別のホストの `forge-runner` がジョブを取りに来て、ジョブのコンテナが対象のリポジトリを fetch し、成果物を出す。今の主体は人・個人のトークン・サービスアカウント・代理（`Credential::Session` / `Token` / `ActAs`）だけで、どれも CI に合わない。

- サービスアカウントのトークンは長命でプロジェクトをまたぎ、漏れたときの範囲が広い。1 つのジョブ・1 つの SHA に絞れない。
- AI の複製の拒否（ADR-0026 の 2026-11-10 の決定、FG-AUTH-0022）は `Principal::is_agent_audience()` が真の主体に、既定のブランチの先端に機密の文書があるリポジトリの clone・fetch を断る（`authorize.rs` の `RepoRead` × `RepoCopy` の arm）。CI のジョブを今の主体（サービスアカウント）で表すと、**機密の文書のあるリポジトリで CI が動かない**。
- 逆に CI に機密の内容を読ませると、ログ・成果物・診断（計画 04 P-9）を通って AI に渡る道ができる。AI が書いた `.forge-ci.yml` のスクリプトは中身を出力に書ける。
- `is_agent_audience()` は文脈 API の FG-AUTH-0021（ADR-0023 Amendment X5）も使う。意味を変えると両方の境界が動く。

壊れたときに、どの主体がどの規則で通った・断られたかをログだけで言えることが要る。

## Decision

1. **新しい資格情報を 2 つ足す**（`forge-auth/src/model.rs` の `Credential`）。
   - `Credential::Runner(RunnerCredential)`: 管理者が登録した Runner。**Runner の API の Action だけ**（ジョブの取得の long-poll・心拍とリース・ログの追記・完了）。リポジトリ・API の読み取りは一切できない。トークンは ADR-0019 と同じ形（SHA-256 のハッシュだけを `ci_runners` に保存、平文は Runner のホストのファイル、ADR-0013）。
   - `Credential::Job(Box<JobCredential>)`: ジョブを Runner に割り当てたときに 1 つ発行する短命のトークン。**1 つのプロジェクト・固定した SHA・そのジョブの生存期間**に結びつく。期限は `ジョブの時間切れ + 猶予`、ジョブが終わる（成功・失敗・取り消し・リースの喪失）と同じトランザクションで `revoked_at` を書く。要求のたびに DB で失効を確かめる（署名だけの JWT にしない）。
   - どちらも人の `UserId` を持たず、`admin_rights()` は常に false。`Principal` は forge-web の認証だけが作る（`Principal::runner` / `Principal::job`。試験 `only_forge_web_builds_ci_principals`）。
2. **判定は新しい `job.rs` で、`authorize()` の 1 か所の中**。`authorize()` は `Credential::Runner | Job` を最初に `job::decide` へ渡す（`act_as::decide` と同じ形）。許可は `Action::ci_rule() -> CiRule { Denied, Runner, Job }` の 1 つの網羅的な `match` で、**既定で閉じる**。`Denied` の Action はすべて行列に `ci_denied_for_<action>` の行を生成する。
3. **ジョブの複製は自分のプロジェクトの固定した SHA だけ、機密の印に関係なく許す**。新しい `Resource::PinnedCopy { project, sha }`（`authorize_pinned_copy` だけが作る）で、`job.rs` は `project == credential.project` かつ `sha == credential.sha` かつジョブが生きていることだけを見る。`CopyMark` は見ない。Job が `RepoCopy` や `Project` で求めたら、他のプロジェクト・他の SHA と同じく FG-REPO-0005 で断る（`reason=job_requires_pinned_copy` はログだけ。C0 のレビューで、見えるプロジェクトだけ 403 になる存在の手がかりを閉じた）。Smart HTTP の経路はジョブに隠した ref `refs/pipelines/<pipeline_id>/head` だけを広告する（`transfer.hideRefs` で他を隠す）。届くのはその SHA とその祖先だけ。
4. **`is_agent_audience()` は変えない**。Job・Runner の主体は人ではないので真を返すが、`job.rs` が先に決めるので `RepoCopy` の arm にも文脈 API にも届かない（`ContextRead` などは `CiRule::Denied`）。
5. **出力は元のリポジトリの印を継ぐ**。ログ・成果物・診断・失敗の指紋を読む Action（`CiLogRead`・`CiArtifactRead`・`PipelineDiagnosisRead`）は `Resource::CiOutput { project, pipeline_mark, tip_mark }` で判定する。`pipeline_mark` はパイプラインを作るときにその SHA を測った値（`ci_pipelines.copy_mark`、測れなければ `Undecided`）、`tip_mark` は読むときの既定のブランチの先端。人は `RepoBrowse` と同じ。**エージェントの相手は両方が `Open` のときだけ**。それ以外は FG-AUTH-0022（`route` = `ci_log` / `ci_artifact` / `ci_diagnosis`、`reason` = 印の名前、`mark_of` = `pipeline` / `tip`）。
6. **サービスアカウントは今のまま**: CI の主体にしない。エージェントの相手として、機密のリポジトリの複製と CI の出力を断られる。M3 の `ci/gates` のスクリプトは切り替え（計画 14 X）までサービスアカウントで報告を続ける。
7. **チェック `ci/pipeline` は Job も Runner も書かない**。forge の worker が `system` の主体（ラベル `forge-ci`）で DB の経路から書く（`forge/` と同じ形）。`CheckWrite` は HTTP からの `ci/pipeline` を常に断り（`reason=ci_namespace_internal`）、切り替えの後はそのプロジェクトの `ci/` 全体を断る。
8. **Job でできないこと**: push、チェックの報告、MR の操作、シークレットの読み取りの API（シークレットは割り当てのときに Runner へ渡し、保護ブランチのパイプラインだけ。計画 14 C6）、他のパイプラインの成果物、文脈 API。

### Amendment: ADR-0026（2026-11-10、AI の複製の拒否）への追記

ADR-0026 の安全網（FG-AUTH-0022、`RepoRead` × `RepoCopy`）は CI のジョブには届かない。ジョブは決定 3 の `PinnedCopy` で `job.rs` が判定し、印を見ない。代わりに CI の出力が決定 5 で同じ拒否を受けるので、機密の内容が CI を通って AI に渡る道は閉じたまま。運用の規則（機密の文書はエージェントが読めないリポジトリに置く）は変わらない。FG-AUTH-0022 の `cause`・`user_message` は C5 で CI の出力を含む文に改める（`errors.toml` と生成物を同じコミット）。

## Alternatives Considered

### B: CI をサービスアカウントと個人のトークンで動かす（gitlab-runner の形）
- **Pros**: 新しい資格情報が要らない
- **Cons**: トークンが長命でプロジェクトをまたぐ。機密のリポジトリで CI が FG-AUTH-0022 で止まる。止めないために例外を足すと、同じサービスアカウントの API の読み取りも通る
- **Why not**: 漏れたときの範囲を 1 ジョブ・1 SHA に絞れない

### C1: `is_agent_audience()` から Job を外す
- **Pros**: 既存の `RepoCopy` の arm がそのまま通る
- **Cons**: 文脈 API（FG-AUTH-0021）の定義も動く。境界の意味が 2 か所で変わる
- **Why not**: 共有の定義を変えず、Job は専用の判定で決める

### C2: AI が起こしたパイプラインのジョブをエージェントとして扱う
- **Pros**: AI の push が機密の内容に届かない
- **Cons**: AI が push するたびに機密のリポジトリの CI が落ちる（AI 駆動の開発の主な経路が止まる）。漏れの道はジョブの読み取りではなく出力なので、出力を閉じれば足りる。「誰が起こしたか」の判定が自動マージ・再実行・キューで曖昧になる
- **Why not**: 決定 5 で出力を閉じる方が単純で、境界が 1 つ

### Job のトークンを署名だけの JWT にする
- **Pros**: DB を引かない
- **Cons**: ジョブの終わりに即座に失効できない
- **Why not**: 「終わりで失効」を守れない

## Consequences

### Positive
- 機密の文書のあるリポジトリでも CI が動き、AI には出力を渡さない
- 判定は `authorize()` の 1 か所のまま。CI の主体は既定で閉じ、行列の生成の行で全 Action を覆う
- Runner のホストが侵害されても、Runner のトークンではリポジトリを読めず、Job のトークンは 1 ジョブで切れる

### Negative
- `Credential` の変種が 5 つになり、`Principal` の構成子と `reconfirm` の引き継ぎが増える
- CI の出力を読むたびに既定のブランチの先端を測る（ADR-0026 のキャッシュを使う）
- 機密のリポジトリでは AI がパイプラインの診断で自分を直せない（人のアカウントで読む）

### Risks
- **ジョブのスクリプトが外へ送る** → 外への通信は既定で閉じる（計画 14 §9 の 3）。許すのは社内のミラーだけ。`ci-runner-security-reviewer` が定義の変更を見る
- **Runner のホストが全プロジェクトの中身を見る** → 機密のリポジトリ用にタグで Runner を分けられる形にする（C3）。Runner のホストでは AI のエージェントを動かさない
- **隠した ref の広告の漏れ** → `git-protocol-reviewer` が C0 で確かめる。試験 `job_fetch_sees_only_the_pipeline_ref`
- **印の測定の失敗を開いたとみなす** → `Undecided` は閉じる（ADR-0026 と同じ）

## 失敗の場合とログでの見え方

| 失敗 | コード | ログの項目 |
|---|---|---|
| Runner / Job のトークンが無い・期限切れ・失効 | FG-AUTH-0001 | `credential=runner\|job`、`reason=unknown\|expired\|revoked`、`job_id` |
| Job が他のプロジェクト・他の SHA、または固定した複製以外の `RepoRead` | FG-REPO-0005（見えないプロジェクトと同じ 404。見える・見えない・無いで同じ応答） | `reason=job_other_project\|job_sha_mismatch\|job_requires_pinned_copy`、`action`、`job_id` |
| Job の許されない Action | FG-AUTH-0003 | `reason=ci_not_allowed`、`action`、`ci_rule`、`job_id` |
| Runner が他の Runner のジョブ・終わったジョブ | FG-AUTH-0003 / CI-next（JobLeaseLost） | `reason=lease_other_runner\|job_finished`、`runner_id` |
| AI が機密の CI の出力を読む | FG-AUTH-0022 | `route`、`reason`、`mark_of`、`audience`、`pipeline_id` |
| HTTP から `ci/pipeline` を書く | FG-AUTH-0003 | `reason=ci_namespace_internal`、`action=CheckWrite` |

## 後の MR の義務（C0 のレビュー、2026-11-10）

C0 は型と判定だけを入れた（トークンの保存・発行・照会は無い）。次を後の MR が満たす。満たすまで ADR は accepted にしない。

**C3（Runner の API・トークン）**
1. プロジェクトの削除・移動・アーカイブと**同じトランザクションで**、そのプロジェクトのジョブのトークンに `revoked_at` を書く。
2. `JobTokenState::Expired` は **DB の `now()`** と `expires_at` を比べて決める（アプリのホストの時計を使わない）。`authorize()` は時計を持たないので、認証が読んだ状態を `JobCredential.state` に入れる。
3. `JobCredential.sha` は `forge_core::push::ObjectId` で、40 桁（SHA-1）か 64 桁（SHA-256）の**小文字の 16 進だけ**を受ける（`ObjectId::parse` が大文字・短縮・空白を拒む。C0 で確認済み）。`ci_pipelines.sha` も同じ形で保存し、比較は文字列の完全一致（短縮した SHA・ref 名で照合しない）。

C3 で満たした（2026-11-10）: 1 は `projects::delete_decided` と `namespaces::move_subtree`（プロジェクトとグループの移動）の同じトランザクションで
`revoke_project_job_tokens`（試験 `deleting_or_moving_the_project_revokes_its_job_tokens`。アーカイブは機能が無い）。2 は認証が DB の
`expires_at <= now()` を読む（`expiry_is_decided_by_the_database_clock`）。3 は `ci_job_tokens.sha` を `ci_pipelines.sha` から写し、`ObjectId::parse` で読む。
ジョブの終わり（完了・取り消し・リースの喪失）でも同じトランザクションで失効する。

**C5（CI の出力）**
4. ログ・成果物・診断のどの経路よりも先に、`Undecided`（測れなかった）の印でエージェントの相手の CI の出力の読み取りが断られる（閉じる）試験を入れる。

C5 で満たした（2026-11-11、成果物は C6 が同じ `authorize_ci_output` を使う）: 4 は `an_undecided_mark_refuses_every_agent_audience_every_ci_output`
（forge-auth）、`a_refused_decision_reads_nothing_and_answers_its_error`・`ci_log_text_is_read_only_after_a_decision`（forge-db: 判定の前に
本文・抜粋を読まない）、`an_agent_is_refused_the_logs_of_a_marked_pipeline_or_tip`（forge-web: ルート）。`Resource::CiOutput` は決定 5 の
2 つの印に加えて `declared_agent`（`Forge-Context-Audience: agent`。`gstn mcp` は常に送る）を持ち、人のトークンでも MCP の経路では AI として
判定する（`audience=header`）。先端を測れない（Smart HTTP が無い、リポジトリの準備中、測定の失敗）ときは `Undecided`。

**C3 / C6 / C11 / C12（C2 のレビュー、2026-11-10 で追加）**
5. **シークレットを渡すかは作成時の記録だけで決める**: `ci_pipelines.source = 'push'` かつ `ci_pipelines.protected`（作成のときに保護されたブランチだったか、
   C2 で保存）のときだけ。`manual` / `merge_request` / `schedule` のパイプラインには **決して** 渡さない。実行の時点の保護の設定や ref 名から判定し直さない。
   C2 は `StoredPipeline::secrets_allowed()` とその試験（`only_a_protected_push_may_release_secrets`）を置いた。C3・C6 はこの関数だけで判定し、
   `secrets_allowed()` が偽のパイプラインのジョブにシークレットが渡らない試験を Runner の API の側にも入れる。
6. **保護された push のパイプラインの再実行**（`PipelineRetry`）は、シークレットを持つジョブもやり直す。Developer・エージェント・サービスアカウントに許すが、
   **デプロイのジョブ（C11）はそれとは別に人だけの `DeployApprove` を要る**（再実行が承認の代わりにならない）。C11 は「再実行されたデプロイのジョブが承認を待つ」試験を入れる。
7. 手動・定期の実行の保護（C12）: 今は常に保護されない扱い。C12 で変えるときも 5 の判定（作成時の記録）を通す。
   **C12 で決めた（2026-11-13、下の Amendment）**: 手動・定期のパイプラインは保護されないまま（シークレットは渡らない）。定期の実行は持ち主の今の権利で判定する。

## 追記（2026-11-10、C1）— `forge-ci` の置き場所と、シークレットの判定

- `forge-ci`（`.forge-ci.yml` の解析・lint・plan）は機能クレートではなく **純粋な層**（`forge-core → forge-ci → forge-doc → forge-errors`、契約 9、ADR-0020 の
  Amendment）。`gstn` と `forge-checks` が同じ lint を使える。パイプラインの作成と実行は後の機能クレート（仮称 `forge-pipelines`、C2）。
- シークレットは二重に止める。lint: シークレットを持つジョブは `merge_requests: false` で、ブランチのパターンがリポジトリの保護ブランチに覆われていること
  （`LintContext::with_protected_branches`。保護の一覧を知らない手元の検査 `LintContext::local_check` ではどのブランチも保護されていない扱い）。
  plan: `Trigger.protected` が偽（保護されていないブランチ）か MR のパイプラインで、選ばれたジョブがシークレットを持てば計画が失敗する
  （`secret_on_unprotected_ref`。黙って外さない）。C6 も保護ブランチにしか値を渡さない。

## 追記（2026-11-11、計画 14 C5s）— ジョブのサービスとネットワーク

- ジョブは `services`（2 つまで、`{ name, image, env?, user?, tmpfs? }`）で横にコンテナを動かせる。GitStation の試験は PostgreSQL が要り
  （`FORGE_TEST_DATABASE_URL`）、ジョブのコンテナは `--network=none` なので、これが無いと `ci/pipeline` を必須にできない（X）。
- **ネットワークは閉じたまま**。ジョブにサービスがあると Runner はポッドを `podman pod create --network=none` で作り、サービスとジョブの
  コンテナを同じポッドに入れる。共有するのは**ループバックだけの network namespace**（2026-11-12 の追記で `--share=net`。それまでは pod の既定の ipc / uts も共有した）: 届くのは `127.0.0.1`
  の互いだけで、外・Runner のホスト・社内のネットワークには届かない。DNS も無い。ジョブだけのときは従来どおりポッドを作らない。
- サービスのコンテナはジョブと同じ閉じ方（読み取り専用のルート、`--cap-drop=all`、`no-new-privileges`、同じメモリ・プロセス数・ulimit、
  `--pull=missing`）。書ける場所は `/tmp` と、ファイルが宣言した `tmpfs`（4 つまで、絶対パス、`/proc`・`/sys`・`/dev` の下と `/` を断る、
  `mode=1777`）だけ。権限を足す口（`command`・`ports`・`volumes`・特権・能力）は無く、未知のキーは lint が断る。root でない `user` を勧める:
  `user` が `root`・`0`・`0:<gid>` なら lint は**警告** `service_user_root`（エラーにはしない。rootless Podman ではコンテナの root もホストの
  Runner の利用者に写り、能力は無い。`gstn ci lint` は警告を出して exit 0）。
- **上限**: ポッドの cgroup にジョブの `memory` を付ける（`pod create --memory --memory-swap`）ので、ジョブとサービスの合計もジョブ 1 つ分を
  超えない（各コンテナにも同じ値）。Podman のポッドには `--pids-limit` が無いので、プロセス数はコンテナごとの上限（合計は最大 3 倍）。
- **時間**: ジョブの `timeout` はコンテナの段の全体に 1 つの期限（ポッド・サービス・pull・スクリプトが残りを分け合う）。
- **infra**: `--infra-image` は付けない。rootless Podman は pause のイメージをその場で作り（`localhost/podman-pause:<版>`、catatonit）、
  取得しない。本物の Podman の試験がその名前を確かめる。ポッドとジョブのコンテナは同じ名前（`forge-job-<job>-<attempt>`。Podman では
  ポッドとコンテナの名前は別で、同じ試験が確かめる）。
- **後始末**: ポッドを消せなければ 1 回やり直し、それでも残れば FG-CI-0007 `reason=pod_remove` で Runner は次のジョブを取らない
  （作業ディレクトリが消せないときと同じ）。
- イメージはジョブと同じ規則（ダイジェストで固定、Runner の `image_registries`）。サービスは要求（claim）に無く、Runner が
  `definition_hash` で確かめた `.forge-ci.yml` から読むので、許されないサービスのイメージ（FG-CI-0011、`service`）は fetch の後・
  コンテナと pull の前に断る。
- `env` の値はファイルの平文で argv の 1 要素（`--env=K=V`、シェルを通らない）。**シークレットではない**（C6 のシークレットはサービスに
  渡さない）。lint は不正な値を出力に書かない。
- Runner は準備を待たない（ジョブのスクリプトが待つ）。ポッドやサービスを起動できなければ FG-CI-0014、ポッドはどの終わり方でも
  `pod rm --force` で消し、起動のときも残りを消す。ジョブが失敗したら各サービスのログの最後の 50 行をジョブのログに足す（ジョブの出力と同じマスク）。

## Amendment 2026-11-12 — ジョブのコンテナを containers.conf に任せない（FG-CI-0015）

- **見つけたこと**（GitStation 自身の CI を試験環境の Runner で動かして）: Runner のイメージ quay.io/podman/stable は
  `~/.config/containers/containers.conf` に `volumes = ["/proc:/proc"]`、`/etc/containers/containers.conf` に `ipcns`・`utsns`・
  `cgroupns = "host"` を持つ。Runner はこれらを Podman の既定に任せていたので、ジョブとサービスのコンテナが Runner の /proc
  （Runner・他のジョブ・サービスのすべてのプロセス）を見られ、Runner の IPC・UTS の namespace を共有した。決定 5 の「ジョブの境界」の欠陥。
  レビュー（ci-runner-security-reviewer）で、containers.conf は他にも `env`・`env_host`・`devices`・`default_sysctls`・`annotations`・
  `mounts.conf`・OCI hooks でジョブを広げられることが分かった。
- **決定**:
  1. Podman は **Runner 自身の containers.conf だけ**を読む。`<work_dir>/runner/podman/containers.conf` を起動のたびに書き直し
     （閉じた値: `volumes`・`devices`・`default_sysctls`・`annotations` は空、`env_host`・`http_proxy`・`init` は false、namespace は
     private、`shm_size = "64m"`、ホストの名前を `/etc/hosts` に入れない、空の `mounts.conf` と空の OCI hooks の場所）、すべての
     Podman の呼び出しに `CONTAINERS_CONF` で渡す。これがあると Podman 5.3 はシステム・利用者の containers.conf も drop-in も読まない
     （試験環境で確かめた）。Podman の呼び出しの環境は `PATH` と `CONTAINERS_CONF` だけ（従来から環境を消している）。
  2. 各コンテナにもフラグで同じことを付ける: `--pid/--ipc/--uts/--cgroupns=private`・`--env-host=false`・`--http-proxy=false`・
     `--init=false`・`--shm-size=64m`。ポッドは `--share=net`（ループバックだけを共有。infra の ipc / uts は共有しない）。
  3. **ジョブを取る前に毎回**（と起動のとき・`check`）確かめのコンテナを作って `podman inspect` を読み、マウント・バインド・デバイス・
     能力・特権・SecurityOpt・sysctl・namespace・user namespace・`/dev/shm`・注釈・想定外の環境変数のどれかが違えば FG-CI-0015 で
     ジョブを取らない。containers.conf の `volumes` は**フラグで外せない**（ジョブ自身の `--volume` があっても足され、`podman inspect`
     には出ない）ので、確かめのコンテナには `--volume` を付けない。
- **選ばなかったもの**: `--userns=private`（rootless では uid の対応の指定が要る）・`--userns=auto`（入れ子の試験環境で
  `gid_map` を書けない）— rootless Podman の user namespace（Runner の利用者の subuid）のまま。`--no-hosts` — `/etc/hosts` の無い
  イメージ（ci/rust・ci/postgres）で `localhost` が引けなくなる（代わりに `base_hosts_file = "none"`・`host_containers_internal_ip = "none"`）。
  `CONTAINERS_CONF_OVERRIDE` — 他の containers.conf の上に重ねるだけで、Runner の知らないキーが残る。確かめのコンテナを起動して
  `/proc` を読む — 起動できるイメージが要る。
- **既存の Runner への影響**: ホストの containers.conf（`[engine]` を含む）は Runner のジョブに効かなくなる（`cgroup_manager`・`runtime`
  などを変えていたホストでは Podman の既定に戻る）。`storage.conf`・`registries.conf` は従来どおり。ジョブとサービスは同じポッドでも
  IPC（共有メモリ）を共有しない。`/etc/hosts` にホストの名前・IP・`host.containers.internal` が入らない。ジョブを取る前に毎回 Podman を
  4 回呼ぶ（約 1 秒）。
- runbook `ci-runner`（「ジョブの切り離し」）、FG-CI-0015 JobIsolationUnsafe（新規、alert）。

## Amendment 2026-11-12（計画 14 C6）— CI の変数（シークレット）

- **保存**: 表 `forge.ci_variables`（プロジェクトごと、名前は `[A-Z][A-Z0-9_]*`、`CI`・`PATH`・`HOME`・`HOSTNAME`・`FORGE_*`・`CI_*` は Runner が
  自分で設定するので断る）。値は ADR-0013 の秘密のファイル `ci.secret_keys_file`（`<key_id> <64 桁の 16 進>`、いちばん大きい `key_id` で封じ、
  並んだ鍵はすべて開ける）の鍵で AES-256-GCM で封じ、関連データで (プロジェクト, 名前) に結びつける（別の行に写した暗号文は開かない）。
  読み取り用の接続は暗号文を読めない。監査はプロジェクトに `ci_variable.created` / `.updated` / `.deleted`（値も暗号文も入れない）。
- **管理**: `CiVariableManage`（Maintainer 以上、**人だけ**。エージェント・サービスアカウント・ボット・代理・Runner・Job は断る）。
  `GET|PUT|DELETE /api/v1/projects/{id}/ci/variables[/{name}]`、`gstn ci variable list|set|delete`（値は標準入力からだけ）。
  **値はどのルートも書いた後に返さない**。書き込みはロックしたプロジェクトで判定をやり直す。
- **渡す**（決定 8 と義務 5）: Runner の取得のときだけ、`StoredPipeline::secrets_allowed()`（`source = push` かつ作成のときに保護）の
  パイプラインのジョブに、そのジョブが `secrets:` に挙げた名前だけ。渡せないジョブは **Runner に渡さずにその場で失敗にする**
  （黙って外さない）: 許されないパイプライン FG-CI-0017、登録の無い名前 FG-CI-0018、鍵が無い・消えた・開かない FG-CI-0016（alert）。
  `protected_only`（既定で真）は今は範囲を広げない（偽でも義務 5 が優先）。後で広げるときに移行が要らないように列だけ置いた。
- **Runner**: 名前の集合が固定した SHA の `.forge-ci.yml` のそのジョブの `secrets:` と一致し、値が 1 行であることを確かめ（違えば
  FG-CI-0008 `secrets_mismatch` / `secret_invalid`、コンテナは作らない）、`<work_dir>/job/secrets/job.env`（0700 / 0600、どこにも
  マウントしない）に書いて `podman create --env-file` に渡し、create の直後に消す。値は argv・Podman の呼び出しの環境・ログに出さない。
  サービスのコンテナには渡さない（C5s のとおり）。
- **マスク**: `masked`（既定で真、8 バイト以上）の値とその一般的な符号化（base64 の標準・URL 安全・パディングの有無、末尾の改行付きの
  base64、パーセント符号化）を `[masked]` にする。Runner は送る前に（書き込みの境目・チャンクの境目にまたがっても見つかるように、
  いちばん長い形 − 1 バイトを手元に残す）、サーバーは保存の前に（そのジョブの値を開いて、チャンクごと）。鍵が使えずマスクできないときは
  チャンクを保存しない（FG-CI-0016）。値を加工した出力（他の符号化、ずれた base64）は見つからない: マスクは安全網で、境界は決定 5
  （出力は印を継ぐ）。
- **doctor** `ci.secret_keys`（鍵のファイルの有無・権限・形、保存された値の `key_id` がすべてファイルにあるか）、runbook `ci-secrets`。
- **受け入れたリスク**: 保護ブランチに push できる主体（エージェントを含む、保護の規則しだい）は、そのパイプラインのスクリプトで値を
  使える。保護ブランチへの push を人に限るのは保護の規則（ADR-0008）の運用。Runner のホストは値を見る（`podman inspect` の環境変数）。

## Amendment 2026-11-12（計画 14 C6）— CI の成果物

- **送る**: スクリプトが成功したジョブの `artifacts: { paths }` の通常のファイルを、Runner が **ジョブ自身のトークン**で 1 ファイルずつ送る
  （`JobArtifactUpload`、`CiRule::Job`、新しい `Resource::JobSelf { project, job }`: トークンのジョブとプロジェクトがパスのものと同じときだけ。
  他のジョブ・他のプロジェクトは見えないプロジェクトの 404、終わったジョブのトークンは FG-AUTH-0001）。リンクはたどらない。サーバーは
  ジョブが `running` の間だけ受け、パスを `check_path` で確かめ、ファイル名は ID から付ける（パスはファイル名にしない）。
- **後のジョブ**（`JobArtifactDownload`、同じ `JobSelf`）: 同じパイプラインの `needs` に挙げたジョブの、今の試行の、成功したものの成果物
  だけ（どれがそうかは読み取りの範囲で決める）。Runner はチェックアウトの後・スクリプトの前に、既存のファイルの上に書かずに置く。
- **人・エージェント**（`CiArtifactRead`）: 決定 5 のとおりログと同じ判定（`authorize_ci_output`、`route=ci_artifact`）。判定の前に行を
  読まない。
- **保持**: 30 日（計画 14 §9 の 6）。`keep`（デプロイした版、C11 で立てる）は消さない。worker の `ci_artifact_expiry`。
- コード: FG-CI-0019 ArtifactTooLarge、FG-CI-0020 ArtifactTransferFailed（runbook `ci-runner` の「成果物」）。

## Amendment 2026-11-12（C6 のレビュー: security-reviewer・ci-runner-security-reviewer）— シークレットを受け取ったジョブの出力

- **値を渡すのは常に保護された push のパイプラインだけ**（義務 5）。`protected_only` は今は何も広げない（偽でも同じ）。
- **渡したことを記録する**: Runner の取得で値を渡すとき、値が forge を出る前に、同じトランザクションで `ci_jobs.secrets_released`
  （一度立てたら戻さない）と、その試行に渡した封じた値の写し `ci_job_secret_snapshots`（`ci_variables` と同じ暗号文、
  (プロジェクト, 名前) に結びつく。読み取り用の接続は暗号文を読めない）を書く。ログのマスクと成果物の検査はこの写しを使うので、
  ジョブの途中で変数を変えても消しても、渡した値はマスクされる。
- **成果物（HIGH）**: そのようなジョブの成果物は、(a) 送られてくる間にマスクの形（下）を探し、見つかれば保存せず FG-CI-0021
  SecretInArtifact（422、Runner はジョブを FG-CI-0020 `server_code=FG-CI-0021` で失敗にする）。(b) 読めるのはプロジェクトの
  Maintainer 以上の**人**だけ（`authorize_ci_artifact`、`Resource::CiOutput.secret_job`）。AI の相手は印に関係なく FG-AUTH-0022
  `reason=secret_job mark_of=job`、サービスアカウント・ボットも同じ。`needs` で受け取れるのも保護された push のパイプラインの中だけ。
- **マスクの形**: 値、base64（標準・URL 安全、パディングの有無、前に 1・2 バイトある位置でも値だけで決まる部分 — `user:secret` を
  まとめて符号化したもの）、末尾の改行付きの base64、16 進（小文字・大文字）、パーセント符号化、JSON の文字列のエスケープ。
  **限界**: 暗号化・圧縮・分割・他の符号化・8 文字未満の base64 の部分は見つからない。だから (b) で読み手を絞る。
- **上限**: 1 ファイル・1 試行の上限と、プロジェクトの割り当て `ci.artifact_quota_bytes`（既定 5 GiB、FG-CI-0022）を、記録の
  トランザクションでジョブの行をロックしプロジェクトの勧告ロックを取って数え直す（並行の PUT で越えられない）。`needs` の
  ファイルが 1000 を超えると FG-CI-0019 `too_many_needed_files`（黙って短くしない）。
- **Runner**: 成果物はコンテナ（とポッド）を消したことを確かめてから送る（消せなければ FG-CI-0020 `container_not_removed`）。
  ファイルはリンクをたどらずに開き、`collect` が見たファイルと同じか（Unix では装置と inode）を確かめる。送受は 1 ジョブで
  30 分の期限（`reason=deadline`）、ファイルの間で停止を確かめる（`reason=stopped`）。心拍は送受の間も続く（試験）。
  `secrets/` はモード 0700 で一度に作り、Runner の起動のときに作業ディレクトリごと消す。Windows（開発機だけ）では env ファイルは
  作業ディレクトリの ACL に頼る。

## Amendment 2026-11-13（計画 14 C12）— 手動のジョブと定期のパイプラインの主体

義務 7（手動・定期の実行の保護）を決める。**どちらもシークレットの範囲は広げない**（義務 5 のまま、作成時の記録 `source = push` かつ
`protected` だけ）。

- **手動のジョブ**（`.forge-ci.yml` の `when: manual`）: `manual` の状態で作り、人が始めるまで動かない。始める Action は
  `PipelineCreate`（手動の実行と同じ権利: Developer 以上、エージェントのトークン・サービスアカウントを含む。計画 14 §4.2）。
  ルート `POST …/pipelines/{pipeline_id}/jobs/{job_id}/play`、監査 `pipeline.job_played`（同じトランザクション、判定をやり直す）。
  ジョブにシークレットを渡すかは**パイプラインの記録だけ**で決まる（保護された push のパイプラインの手動のジョブには渡る。決定 6 の
  再実行と同じ）。デプロイのジョブは別に人の `DeployApprove`（C11）が要るので、手動で始めることが承認の代わりにはならない。
  何も手動のジョブを待たない（`needs` で指すと lint の `needs_manual_job`、ステージの壁からも外す）。パイプラインの状態は他の
  ジョブで決まり、手動のジョブだけのパイプラインは `skipped`。始めると `running` に戻る。
- **定期のパイプライン**: 表 `ci_pipeline_schedules`（監査あり）。作る・変える・消すのは新しい Action `PipelineScheduleManage`
  （Maintainer 以上、**人だけ**。エージェントのトークン・サービスアカウント・ボット・代理・Runner・Job は断る）。読むのは `PipelineRead`。
  **保存した人が持ち主**になり、定期の実行はその持ち主の権利で動く。
  - **判断**: 定期の実行は「人の主体」を新しく作らない。worker（`system`）が、実行のたびに**持ち主を Web のセッションの人として**
    `PipelineCreate` を `authorize()` で判定する。判定はパイプラインを書くトランザクションの中で、ロックしたプロジェクトとアカウントの
    事実で行う（`forge_pipelines::schedules::owner_decision`）。無効になった・抜けた・Reporter 以下になった・削除待ちのプロジェクトなら
    何も作らず FG-CI-0026 ScheduleOwnerNotAllowed（warn）を記録し、定期の行に `refused` を残す。**持ち主が今持つ権利より多くの権利で
    動くことはない**。保存したときの権利を写さない（時間が経つと古くなるため）。
  - パイプラインは `source = schedule`、`triggered_by` = 持ち主、**常に保護されない**（表の CHECK `ci_pipelines_protected`）。だから
    シークレットを名指すジョブが選ばれると計画は `invalid`（`secret_on_unprotected_ref`）で、値は一切渡らない。持ち主が Maintainer でも同じ。
  - 監査: 人の変更は `pipeline_schedule.created` / `.updated` / `.deleted`（持ち主・ブランチ・規則）、実行は `pipeline.scheduled`
    （行為者 `system`、`owner_id`・`schedule_id`・`pipeline_id`）を同じトランザクションで書く。
  - 時刻: 5 つの欄の cron を固定の UTC オフセットで読む（日本・インドネシアは夏時間が無い）。5 分に 1 回より頻繁な規則は断る。
    期限は DB の `now()` で決め、止まっていた間の取りこぼしは 1 回だけ動かす。1 つの期限は 1 つのジョブ（`ci_schedule:<id>:<due>`）。
- **選ばなかった案**: (a) 定期の実行をサービスアカウントで動かす（長命で広い主体を作る。持ち主の降格が反映されない）。
  (b) 保存した時点の権利を記録して使う（降格・無効化の後も動き続ける）。(c) 定期のパイプラインを保護された扱いにしてシークレットを渡す
  （人が見ていない時刻に保護ブランチのシークレットが自動で使われる。必要になったら別の ADR で、`DeployApprove` と同じ人の承認を前提に決める）。
- **C12 のセキュリティレビュー（2026-11-14）で改めたこと**:
  - 保護された push のパイプラインの手動のジョブはシークレットを受け取りデプロイもできるので、始めるには `PipelineCreate` に加えて
    新しい Action `PipelinePlayProtected`（Maintainer 以上、人だけ）を、行のロックの下で読んだ `protected` の印で判定する。
    Developer・エージェントのトークン・サービスアカウント・ボットは断る（FG-AUTH-0003）。保護されないパイプラインは今までどおり。
  - 持ち主の判定で断られた定期は止める（`active = false`、`pipeline_schedule.deactivated`、行為者 `system`、`cause_code`）。
    Maintainer が保存し直すと引き継ぐ。持ち主が変わるのはブランチ・規則・オフセットを変えたとき（と止まった定期の保存）だけで、
    説明の変更や停止・再開では変わらない。
  - 同じ定期の前のパイプラインが `pending` / `running` の間は始めない（`busy`）。定期のパイプラインは 1 時間にプロジェクトで 12、
    全体で 120 まで（超えると `capped`、FG-CI-0028 warn）。
  - `spec.released` は (プロジェクト, タグ) で 1 回だけ（タグを作り直しても再び出ない）。タグを push したのが**人**（人の
    アカウントで、エージェントのトークンではない。サービスアカウント・ボットは不可）で、そのタグを覆う保護の規則があるときだけ出す。

## Amendment 2026-11-14（計画 17 P17-1）— GPU の Runner とモデル置き場

- **背景**: 計画 17 §1.6 の評価ベンチは、ジョブの `services` に Ollama を入れ（C5s のポッド、ループバックだけ）、GPU と読み取り専用の
  モデル置き場で本物のモデルを動かす。ジョブのコンテナは `--network=none` のままで、外への通信は増やさない。
- **決定**:
  1. **`.forge-ci.yml` は求めるだけ**。サービスの `gpu: true`（`true` / `false` だけ）。装置の名前・ホストのパス・マウントを書くキーは
     無い（未知のキーは `key_unknown`）。`gpu: true` のサービスを持つジョブは `tags` に `gpu-eval` が要る（lint の
     `service_gpu_without_tag`、FG-CI-0001）。
  2. **GPU とモデル置き場を決めるのは Runner のホストの設定だけ**: `[gpu_eval]` の `devices`（CDI の名前だけ。`/dev/...` は
     設定の検査で断る）と `[gpu_eval.model_store]`（ホストの絶対パス `path`・コンテナの中のパス `target`・マニフェストの
     sha256 `manifest_sha256`）。
  3. **渡すのは求めたサービスのコンテナだけ**: `--device=<CDI 名>` と `--volume=<path>:<target>:ro,nosuid,nodev,noexec`。
     **ジョブ自身のコンテナには決して渡さない**（`Container` に GPU の項目そのものが無い）。同じジョブの他のサービスにも渡さない。
     閉じ方（読み取り専用のルート、`--cap-drop=all`、`no-new-privileges`、各 namespace、上限）はそのまま。
  4. **ポッドは `--network=none` のまま**。GPU のサービスもネットワークを得ない（`--network` を付けない）。
  5. **タグ `gpu-eval` の Runner はそのタグを求めたジョブだけを取る**（forge-db の取得の文に「Runner の排他のタグ」
     `EXCLUSIVE_RUNNER_TAGS = ["gpu-eval"]`）。タグの無いジョブ・他のタグだけのジョブは GPU の Runner に決して割り当てられず、
     他の Runner での割り当てとコンテナの設定は今と同じ。
  6. **モデル置き場はダイジェストで確かめてから渡す**（Runner の起動のときと、それを渡すジョブのたびに、ジョブの期限の中で）:
     `<path>/forge-models.sha256`（sha256sum の形）の sha256 が設定の `manifest_sha256`、列挙したすべてのファイルがその sha256、
     列挙の外のファイル・リンク・特殊ファイルが無い、グループ・他人が書けず Runner の利用者のものでもない。違えば FG-CI-0030
     （alert）でジョブは失敗し、ポッドもコンテナも作らない（起動のときは Runner が止まる）。
  7. 求めたのに Runner のホストに `[gpu_eval]` が無ければ FG-CI-0029（warn）。チェックアウト・ポッド・コンテナの前に失敗にし、
     GPU 無しで走らせることはしない（`forge/ai-eval` は `error` になり、合格にはならない）。
- **選ばなかったもの**: `.forge-ci.yml` に装置やパスを書かせる（リポジトリを書ける人がホストの資源を選べてしまう）。ジョブの
  コンテナに GPU を渡す（ジョブのスクリプトは MR の作者のコードで、GPU のドライバの面を広げる）。モデルを毎回レジストリから pull
  （外への通信、または社内レジストリに数十 GB）。マウントの再ラベル（`z` / `Z`）— 共有のモデル置き場を Runner が書き換えることになる
  （SELinux のラベルは管理者が一度付ける。runbook）。起動のときだけ確かめる — 起動の後の書き換えを見逃す。
- **既存の Runner への影響**: `[gpu_eval]` の無い Runner・タグ `gpu-eval` の無い Runner は何も変わらない。GPU のジョブは毎回
  モデル置き場の全ファイルの sha256 を読む（数十 GB で数十秒。ジョブの `timeout` に含まれる）。
- **レビュー（ci-runner-security-reviewer、2026-11-15）で足したこと**:
  - **プロジェクトの許可と同時実行の上限**（サーバーの設定）: `ci.gpu_eval_projects`（プロジェクトの id の一覧、既定は空 = どの
    プロジェクトも使えない）と `ci.gpu_eval_max_running`（1 プロジェクトで同時に動く `gpu-eval` のジョブ、既定 1、1〜16）。
    一覧に無いプロジェクトの `gpu-eval` のジョブを持つパイプラインは作成のときに `invalid`（FG-CI-0001 `gpu_eval_not_allowed`、
    ジョブの名前）。取得の文も同じ条件を持つ（一覧に無いプロジェクトのジョブ・上限に達したプロジェクトのジョブは取らない）。
    GPU の Runner の取得は勧告ロックで 1 つずつにし、数えた上限が並行の取得で破れないようにする。
  - **モデル置き場の確かめを強める**: 設定の読み込みでパスを一度だけ正規化し（`realpath`）、それを確かめてマウントする。
    確かめのたびに、パスが正規のままか（`not_canonical`）、上のすべてのディレクトリが root のもので、リンクでなく、グループ・
    他人が書けないか（`ancestor_unsafe`）を見る。各ファイルは `O_NOFOLLOW` で開き、開いたハンドルの装置・inode が走査で見た
    ものと同じで、リンクが 1 つであること（`replaced`・`hardlink`）。Runner の利用者だけでなく、その subuid の範囲（ジョブの
    コンテナの利用者）のファイルも断る（`owner_untrusted`）。**置き場の持ち主は信頼する**（持ち主の権限があれば確かめと読み取りの
    間に書き換えられる。それ以外の誰も — Runner も、ジョブも — 書けないことを確かめる）。
  - 確かめには**ジョブの `timeout` と別の期限**（ジョブの前 30 分、起動のとき 1 時間）。超えたら FG-CI-0030 `reason=deadline`
    （時間切れの FG-CI-0010 ではない）。
  - **ループバックの共有（受け入れた残り）**: ジョブのコンテナは GPU のサービスとポッドのループバックを共有する。ジョブは
    Ollama の API（`127.0.0.1:11434`）を呼べ、それが評価ベンチの目的。ジョブが GPU の装置そのものやモデル置き場のファイルに
    触れることはない。より狭くするなら unix ソケットだけを共有のボリュームで渡す形があり得る（今はしない）。
- runbook `ci-runner`（「GPU の Runner とモデル置き場」）、FG-CI-0029 GpuUnavailable（新規、warn）、FG-CI-0030 ModelStoreUnverified
  （新規、alert）。

## Amendment 2026-11-25（計画 21 G2）— ジョブの取得の深さ（`fetch_depth`）

- **背景**: 計画 21 の追記だけの確かめ（I/O の意味の辞書、ファームウェアの設定のスキーマ）は、ジョブの中で 1 つ前のコミット `HEAD^1` と比べる。
  Runner の取得は `--depth=1` で、ジョブにネットワークは無い（C5s）ので、ジョブは前の版を見られなかった。
- **決定**:
  1. `.forge-ci.yml` のジョブの `fetch_depth`（10 進の整数 1〜50、既定 1）。範囲の外・整数でない値は FG-CI-0001
     `fetch_depth_out_of_range`、1 つの値でないものは `field_invalid`。自動修正の fixer は常に 1。
  2. **深さを決めるのは固定した SHA の定義だけ**: Runner は今のとおり `--depth=1` で取って定義を読み、クレームの `definition_hash` と
     合わせてから、`fetch_depth` が 2 以上ならパイプラインの ref を **もう一度 `--depth=<n>` で取る**（同じ argv・同じトークンの扱い、
     値は数で、`1..=50` に Runner でももう一度丸める）。取った後に ref が固定した SHA のままかを確かめ直す（違えば FG-CI-0009
     `step=verify reason=sha_mismatch`）。取得の失敗は FG-CI-0009 `step=deepen`。クレームの形・表は変えない（サーバーの判定も変えない）。
  3. **取れるのはパイプラインの ref から届く履歴だけ**: サーバーの want の判定（パイプラインの ref から届くコミットだけ）はそのまま。
     深さは want を広げず、固定した SHA の祖先を最大 50 世代まで送るだけ（届かないコミットは深さに関係なく断る。試験）。MR のパイプラインはマージの結果で動き、その第 1 の親は目標の
     ブランチの先端（ADR-0024 Amendment 2026-11-11）なので、`fetch_depth: 2` の `HEAD^1` は目標の先端になる（試験
     `fetch_depth_2_has_head_parent`）。
- **選ばなかったもの**: 深さを `ci_jobs` に書いてクレームで渡す — 表とクレームの形が増え、Runner は結局ファイルと突き合わせる。
  最初の取得を `--depth=<n>` にする — 定義を読む前に深さを知る手段がクレームしか無い。上限なし・`--unshallow` — 大きなリポジトリで
  取得の時間と作業ディレクトリの上限を読めなくなる（50 は追記だけの確かめに 2 で足りる余裕）。ジョブにネットワークを与えて `git fetch`
  させる — C5s の切り離しを崩す。
- **受け入れたリスク**（ci-runner-security-reviewer、2026-11-25 の MEDIUM）:
  - ジョブは最大 50 世代の過去のファイル（パイプラインの ref の祖先、そのブランチを読める人には見えるもの）を読める。ジョブのトークンの
    範囲は今と同じプロジェクトの同じ SHA の履歴で、他の ref・他のプロジェクトには届かない。
  - **出力の印は固定した SHA（先端）の印だけ**（決定 5 のまま）。履歴の中にだけある内容（後で消した・後で印を強めたファイル）を
    ジョブがログや成果物に出すと、その出力は先端の印を継ぐ。`fetch_depth` を求めるのはリポジトリを書ける人で、そのスクリプトは
    MR のレビューを通る（CODEOWNERS）。追記だけの確かめは ID・key と理由だけを出す（計画 21 の例）。履歴の印を見る必要が出たら、
    `fetch_depth` が 2 以上のジョブを `Open` の先端に限る（後の MR）。
  - 2 回目の取得は `limits.fetch` の期限をもう一度使う（ジョブの `timeout` の前に、最悪で取得の期限の 2 倍 Runner を使う）。
    深さ 50 の上限と、浅い取得の 2 回目は差分だけを送ることで抑える。
- 新しいエラーコードは無い（FG-CI-0001 の `reason` が 1 つ、FG-CI-0009 の `step` が 1 つ増える）。

## Error codes and runbooks affected

- 既存のコードを使う: FG-AUTH-0001・0003・0022（0022 は文面の改訂、C5）。新しいコードは CI-next（JobLeaseLost、C3 で登録）。
- C5s: FG-CI-0014 ServiceStartFailed（新規）、FG-CI-0011 に `service`、FG-CI-0001 にサービスの理由。
- 2026-11-12: FG-CI-0015 JobIsolationUnsafe（新規、alert。runbook `ci-runner` の「ジョブの切り離し」）。
- C6: FG-CI-0016 SecretKeyUnavailable（alert）、FG-CI-0017 SecretOnUnprotectedRef、FG-CI-0018 SecretNotDefined（info）、runbook `ci-secrets`（新規）。FG-CI-0019 ArtifactTooLarge（info）、FG-CI-0020 ArtifactTransferFailed（warn、runbook `ci-runner`）。C6 のレビュー: FG-CI-0021 SecretInArtifact（warn、runbook `ci-secrets`）、FG-CI-0022 ArtifactQuotaExceeded（info）。
- C12: FG-CI-0026 ScheduleOwnerNotAllowed（warn、runbook `ci-pipelines` の「定期の実行」）、FG-CI-0027 ScheduleLimitReached（info）、FG-CI-0028 ScheduleRateLimited（warn、レビューで追加）。FG-CI-0003 に `operation=play`。
- runbook: `ci-runner`（新規、C3。C5s で「ジョブのサービス」の節）、`smart-http`（ジョブの fetch の節、C0）、`checks-reporting`（`ci_namespace_internal`、X）。

- 計画 17 P17-1（2026-11-14）: FG-CI-0029 GpuUnavailable（warn）、FG-CI-0030 ModelStoreUnverified（alert）、FG-CI-0001 に `service_gpu_without_tag`、FG-CI-0008 に `gpu_without_tag`。
- runbook: `ci-runner`（新規、C3。C5s で「ジョブのサービス」の節、P17-1 で「GPU の Runner とモデル置き場」の節）、`smart-http`（ジョブの fetch の節、C0）、`checks-reporting`（`ci_namespace_internal`、X）。
- 計画 21 G2（2026-11-25）: FG-CI-0001 に `fetch_depth_out_of_range`、FG-CI-0009 に `step=deepen`。runbook `ci-runner` の Diagnose。
- `forge-auth/README.md` の「CI principals」の節（責務・公開 API）は C0 の MR で足す（act-as の節と同じ形）。
