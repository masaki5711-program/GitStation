# Runbook: gitlab-mirror（FG-INT-0013 / FG-INT-0014 / FG-INT-0021）

GitStation のリポジトリを社内 GitLab へ送る **push ミラー**（M3 O2、計画 06 D-2・D-4 §4.11、ADR-0024 Amendment O2）が
失敗した・遅れている・設定できない、のときの手順。**GitStation が正本、GitLab は保険（写し）**。ミラーは push のたびに
`refs/heads/*` と `refs/tags/*` を `--prune` 付きで送る（`refs/merge-requests/*` は送らない）。

## Trigger

- ログに **FG-INT-0013** PushMirrorFailed（alert）: `repository.push_mirror` ジョブが最後の試行（5 回）まで失敗した。
  それより前の試行は warn `forge.mirror.push_failed`。
- ログに **FG-INT-0014** PushMirrorLagging（alert）: 送っていない push が `mirror.lag_alert_secs`（既定 900 秒）より長く待っている。
  worker の 1 分ごとの見回りが、1 つのミラーにつき 1 時間に 1 回まで出す。
- 設定（`gstn mirror set` / `PUT /api/v1/projects/{id}/push-mirror`）で **400 / FG-INT-0021** PushMirrorTargetInvalid。
- `forge doctor` の `mirror.config`（許可リストが空・資格情報の置き場が読めない）、`mirror.status`（失敗中のミラー）、`mirror.lag`（遅れ）。

## Impact

- GitStation の画面・git・MR・連携は止まらない。止まるのは GitLab の写しの更新だけ。
- **GitStation が止まったときの退避先が古くなる**。いま切り替えると、`last_success_at` より後の push が失われる。
- 重大度: 遅れているだけなら SEV4。GitStation 自体の障害で GitLab に切り替える必要があり、写しが古いなら SEV2（incident-commander）。

## Diagnose（読み取りのみ）

1. ログを `error_code` と `repo_id` で探す。どの行も **URL のパスとトークンを出さない**（`origin` = scheme://host:port だけ、
   `stderr` は `user:password@`・トークン・Authorization ヘッダーを伏せた先頭だけ）。
2. ミラーの状態（管理者）:
   ```
   gstn mirror show --project <id> --json
   ```
   `status.pending_since`（送っていない最古の push。NULL なら最新）、`status.last_success_at`、`status.failures`、
   `status.last_error_code`、`status.last_reason`。
   | last_reason | 意味 | 直し方（下の「Mitigation」） |
   |---|---|---|
   | `rejected` | GitLab が ref を拒んだ（保護ブランチ、pre-receive フック、ミラー用アカウントに強制 push の権限が無い）。ログの `ref` が最初の 1 つ | A |
   | `auth` | 401 / 403。資格情報のファイルのトークンが違う・期限切れ・`write_repository` が無い | B |
   | `unreachable` | 名前の解決・接続・TLS・5xx | C |
   | `timeout` | `mirror.push_timeout_secs` を超えた（初回の大きなリポジトリ） | D |
   | `credential_unreadable` | 資格情報のファイルが無い・通常のファイルでない・グループや他人が読める | B |
   | `target_not_allowed` | 宛先のホストが `mirror.allowed_hosts` から外れた | E |
   | `git` | ほかの git の失敗（`exit_code`、`stderr`） | ログを読み、再現する |
3. DB（読み取り）:
   ```sql
   SELECT repo_id, project_id, origin, credential_name, enabled, updated_at FROM forge.push_mirrors;
   SELECT * FROM forge.push_mirror_status WHERE repo_id = <repo_id>;
   SELECT id, state, attempts, run_after, last_error_code FROM forge.jobs
    WHERE kind = 'repository.push_mirror' ORDER BY id DESC LIMIT 10;
   SELECT occurred_at, action, details FROM forge.audit_log
    WHERE action LIKE 'push_mirror.%' ORDER BY id DESC LIMIT 10;
   ```
4. worker の起動のログに `forge.worker.mirror_lane_off` があれば、ミラーのレーンは動いていない（`mirror.credentials_dir` が未設定）。
5. GitLab 側で最後に受け取った版を見る（読み取り）: `git ls-remote <GitLab の URL>` を管理者の端末から（トークンは端末の資格情報で）。
   GitStation の `git ls-remote` と比べる。

## Mitigation（変更を伴う。**本番は承認が要る**）

`[MUTATING — needs approval]` の手順は、利用者の承認の後でだけ行う。

- **A. 拒否**: GitLab のプロジェクトで、ミラー用アカウントに保護ブランチへの強制 push を許す（Settings → Repository →
  Protected branches → Allowed to force push）か、ミラー先の保護を外す。GitLab 側のフックが拒むなら外す。
  `[MUTATING — needs approval]`
- **B. 資格情報**: 新しいトークン（ミラー用アカウント、スコープ `write_repository`）を発行し、資格情報のファイルを置き換える。
  ファイルは `mirror.credentials_dir/<credential_name>`、中身は 1 行 `<ユーザー名>:<トークン>`、**権限 0600、所有者は forge のサービスのユーザー**。
  `[MUTATING — needs approval]`
- **C. 到達できない**: 名前の解決と経路を確かめる。社内 CA の証明書なら `mirror.ca_file` に PEM を置く。
- **D. 時間切れ**: `mirror.push_timeout_secs` を上げる（最大 1500）。`[MUTATING — needs approval]`
- **E. 許可リスト**: `mirror.allowed_hosts` に `host:port` を足し、worker を再起動する。`[MUTATING — needs approval]`

直したら、1 回送る（管理者。`PushMirrorManage`）:

```
gstn mirror sync --project <id>
```

`mirror.status` が ok、`gstn mirror show` の `status.pending_since` が NULL になれば終わり。

## 設定

| キー | 既定 | 意味 |
|---|---|---|
| `mirror.credentials_dir` | 無し | 資格情報のファイルの置き場（絶対パス）。**未設定なら worker のミラーのレーンは止まる**（`forge.worker.mirror_lane_off`） |
| `mirror.allowed_hosts` | 空 | 宛先として許す `host` / `host:port`（ポートが無ければ 443）。空なら設定できない |
| `mirror.allow_http` | `false` | `http` の宛先を許す（試験だけ。doctor が警告） |
| `mirror.ca_file` | 無し | GitLab の証明書を検証する PEM（無ければ OS の証明書ストア） |
| `mirror.push_timeout_secs` | 300 | 1 回の push の時間の上限（30〜1500） |
| `mirror.lag_alert_secs` | 900 | FG-INT-0014 を出す遅れ（60〜86400） |

ミラーの設定は管理者だけ（`PushMirrorManage`）:

```
gstn mirror set --project <id> --url https://<gitlab>/<group>/<project>.git --credential <credential_name>
gstn mirror show --project <id>
gstn mirror sync --project <id>      # 今すぐ 1 回送る
gstn mirror set --project <id> --url … --credential … --disabled   # 止める（設定は残す）
gstn mirror remove --project <id>
```

## GitStation が止まったとき（GitLab への切り替え）

1. `gstn mirror show` か DB の `push_mirror_status.last_success_at` で、写しがいつの時点かを確かめる（GitStation の DB が読めなければ、
   GitLab の `git ls-remote` と開発者の手元の clone の先端を比べる）。
2. 開発者のリモートを GitLab に向ける（`git remote set-url origin <GitLab の URL>`）。kanflow のそのプロジェクトの接続先を GitLab に戻す
   （kanflow は 1 つのプロジェクトで片方だけを読む。計画 06 §4.11）。`[MUTATING — needs approval]`
3. 復旧したら、GitLab で進んだ分を GitStation に取り込んでから（GitStation に push）、リモートと kanflow を GitStation に戻す。
   **戻す前に取り込まないと、次のミラーが GitLab で進んだ分を `--prune` と強制 push で消す。** `[MUTATING — needs approval]`

## Feedback

- ミラーの遅れが業務に効いたら、postmortem で `mirror.lag_alert_secs` と見回りの間隔を見直す。
- 新しい失敗の形は `reason` を足し、`errors.toml` の FG-INT-0013 と、この表を直す。
