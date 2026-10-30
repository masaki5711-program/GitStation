# Runbook: user-offboarding（FG-AUTH-0009 / FG-AUTH-0010）

利用者の無効化（退職・異動・資格情報の漏えい）と、無効化のガードを越えたあとの復旧の手順（ADR-0019 §9–§11 と Amendment D3 MR2）。

## Trigger

- 利用者を無効化する依頼（退職、異動、資格情報の漏えい）。
- `error_code=FG-AUTH-0009` AdministrationConflict（409、ログと CLI の出力）: 無効化・`bootstrap-admin` が拒否された。
  - `reason=last_admin`（`user`）: その利用者が最後の有効な人間の管理者。ボット・サービスアカウント・無効化済みの利用者は数えない。
  - `reason=last_owner`（`user`、`group_id`、`ownerless_groups`）: その利用者が最上位のグループの最後の直接の有効な Owner。
    サブグループは親から Owner を継承するので数えない。
  - `reason=last_owner`（`user`、`project_id`、`ownerless_projects`、D4 MR4）: その利用者がプロジェクトの唯一の有効な Owner
    （直接、または祖先のグループの直接の Owner・個人の名前空間の持ち主として継承）。削除待ちのプロジェクトは数えない。
    グループとプロジェクトの両方に当たるときは `group_id` が出て、件数は `ownerless_groups` と `ownerless_projects` の両方に出る。
  - `reason=admin_exists`（`active_human_admins`）: 有効な人間の管理者がいるので `bootstrap-admin` は使えない。
  - `reason=not_human` / `reason=user_disabled`（`user`）: `bootstrap-admin` に、人間でない・無効化された利用者を指定した。
- `forge doctor` の `users.administration` が `FAIL ... FG-AUTH-0010` AdministrationOrphaned（warn）:
  有効な人間の管理者がいない（`reason=no_admin`）、または直接の有効な Owner のいない最上位のグループがある（`reason=ownerless_group`、`group_ids`）。
  D4 MR4 から、有効な Owner（直接・継承）のいないプロジェクトも `reason=ownerless_project`（`ownerless_projects`、`project_ids`）で出る
  （`forge_db::projects::ownerless`。doctor への配線は D4 の統合で行う）。
- `forge doctor` の `users.administration` が `warn ... no users yet`: 新しいインストールで利用者がまだいない（コードなし）。最初の管理者を作る（下の「復旧」）。

## Impact

- 拒否された無効化（FG-AUTH-0009）は **何も書かない**: 利用者は有効のまま、トークンも失効しない。漏えいの対応中なら、そのまま放置しない。
- FG-AUTH-0010 の状態では、管理者の操作（利用者・トークンの管理）や、そのグループのメンバーの管理ができる人がいない。データは失われていない。

## 本番での実行の承認

この Runbook で使う次のコマンドは本番のデータを変える。本番では **人が承認した手順としてだけ** 実行する（CLAUDE.md §3.7、ADR-0019 §9）。
各手順に `[MUTATING — needs approval]` と書く。

- `forge admin user disable <username> [--force]`
- `forge admin bootstrap-admin --user <username> [--create --email <email> [--display-name <name>]]`
- （参考）`forge admin token issue` / `forge admin token revoke`

どのコマンドも監査の行を 1 行書く（`actor_label = forge-cli:<OS のアカウント>`）。ログ（標準エラー、JSON）には利用者の id とフラグだけが出て、名前は出ない。

**`actor_label` は自己申告**: OS のアカウント名は環境変数 `USERNAME` / `USER` から取るので、実行する人が書き換えられる。ホストの操作者が
信頼の起点であり（ADR-0019 §8）、**「誰が実行したか」の正式な記録は、この手順の承認の記録**（承認者・実行者・日時・`request_id`）である。
承認の記録に、コマンドの出力に出る問い合わせ番号（`request_id`）を残し、監査の行と突き合わせられるようにする。

## Diagnose（読み取りのみ）

1. 対象の利用者が、最後の管理者・最後の Owner かを確かめる（特権のある接続で）。

   ```sql
   -- 有効な人間の管理者
   SELECT id, username FROM forge.users WHERE kind = 'human' AND state = 'active' AND is_admin;
   -- 対象が直接の Owner である最上位のグループと、ほかの有効な直接の Owner の数
   SELECT m.group_id, n.full_path,
          (SELECT count(*) FROM forge.group_members o JOIN forge.users u ON u.id = o.user_id
            WHERE o.group_id = m.group_id AND o.role = 'owner' AND u.state = 'active'
              AND o.user_id <> m.user_id) AS other_owners
   FROM forge.group_members m
   JOIN forge.namespaces n ON n.kind = 'group' AND n.owner_id = m.group_id AND n.parent_id IS NULL
   WHERE m.user_id = <user_id> AND m.role = 'owner';
   -- 対象が直接の Owner であるプロジェクトと、ほかの有効な直接の Owner の数（継承する Owner は、
   -- 祖先のグループの直接の Owner と個人の名前空間の持ち主。FG-AUTH-0009 の project_id で個別に確かめる）
   SELECT m.project_id,
          (SELECT count(*) FROM forge.project_members o JOIN forge.users u ON u.id = o.user_id
            WHERE o.project_id = m.project_id AND o.role = 'owner' AND u.state = 'active'
              AND o.user_id <> m.user_id) AS other_direct_owners
   FROM forge.project_members m
   WHERE m.user_id = <user_id> AND m.role = 'owner';
   ```

2. FG-AUTH-0010 のときは、強制した無効化を監査の記録で探す。

   ```sql
   SELECT created_at, actor_label, request_id, resource_id, details
   FROM forge.audit_log
   WHERE action = 'user.disabled' AND details->>'forced' = 'true'
   ORDER BY id DESC LIMIT 20;
   ```

   `details` の `ownerless_groups`（Owner のいなくなった最上位のグループの数）、`ownerless_projects`（有効な Owner のいなくなった
   プロジェクトの数、D4 MR4）と `left_without_admin` が、その無効化の残した状態。
   強制した記録がないのに FG-AUTH-0010 が出るなら、forge-db の外でデータが変えられている（Runbook `membership-data`、`append-only-violation` も確かめる）。

## Mitigate

### A. 通常の無効化（退職・異動）

1. 対象が最後の管理者なら、先に別の有効な人間を管理者にする。**管理者を付与するコマンド・画面はまだない**（D3 時点）。対象が有効なあいだは
   `bootstrap-admin` も使えない（有効な管理者がいるため）。管理者の交代がどうしても必要なら、B の手順（`--force`）で無効化してから
   C の手順 2 で新しい管理者を立てる。
2. 対象が最上位のグループの最後の Owner、またはプロジェクトの唯一の Owner なら、先に別の有効な利用者をそのグループ・プロジェクトの Owner にする
   （プロジェクトのメンバーを変える HTTP の経路もまだない。forge-db の `projects::add_member` だけがある）。**グループのメンバーを変えるコマンド・画面はまだない**
   （D3 時点。forge-db の `groups::add_member` だけがある）。それまでは、その利用者の無効化は B の手順になり、Owner のいないグループが残る（C の手順 3）。
3. 無効化する。`[MUTATING — needs approval]`（ロールバック: 無効化を戻すコマンドはまだない。失効したトークンは戻らない — 必要なら新しく発行する）

   ```sh
   forge admin user disable <username>
   ```

   出力が `user <username>: disabled` なら完了。有効なトークンはすべて同じ変更で失効する（`revoked_reason = 'user_disabled'`）。

### B. 緊急の無効化（資格情報の漏えい）— ガードを越える

Owner の移譲を待てないときだけ使う。

**最後の管理者の通常の退職・異動も、今はこの経路になる**（A の手順 1）。この経路は、無効化から `bootstrap-admin` までのあいだ
**管理者が 1 人もいない時間** を作る。そのため:

- 承認の時点で **後任（`bootstrap-admin` で管理者にする利用者）を決めて承認の記録に書く**。後任が決まらないうちは実行しない。
- 無効化の **直後に、同じ作業の中で** C の手順 2 を実行する（間を空けない）。
- 管理者を付与するコマンド（grant admin）は **追跡中の後続の作業** で、できたらこの経路を置き換える（先に後任を管理者にしてから、
  ガードを守ったまま無効化する）。

1. 強制して無効化する。`[MUTATING — needs approval]`（ロールバック: なし。孤立した状態は C の手順で直す）

   ```sh
   forge admin user disable <username> --force
   ```

   出力が `disabled (forced)` と、Owner のいなくなったグループの数・管理者のいなくなったことを示す。監査の行の `details` に
   `forced: true`、`ownerless_groups`、`left_without_admin` が残る。
2. すぐに C の手順で復旧する。

### C. 復旧（管理者がいない・Owner のいないグループがある・新しいインストール）

1. `forge doctor` で `users.administration` の行を確かめる（`reason`、`active_human_admins`、`ownerless_groups`、`group_ids`）。
2. 有効な人間の管理者がいないなら、管理者を戻す。`[MUTATING — needs approval]`（ロールバック: 管理者を外すコマンドはまだない。
   誤った利用者を管理者にしたら、正しい管理者を立てたうえでその利用者を A の手順で無効化する）

   ```sh
   # 既存の有効な人間を管理者にする
   forge admin bootstrap-admin --user <username>
   # 新しい人間の管理者を作る（個人の名前空間も作られる）
   forge admin bootstrap-admin --user <username> --create --email <email> [--display-name <name>]
   ```

   有効な人間の管理者がいると `FG-AUTH-0009 reason=admin_exists` で拒否される（何も書かない）。ボット・サービスアカウント・無効化された利用者は
   管理者にできない（`not_human` / `user_disabled`）。監査の行は `user.admin_bootstrapped`（`created`、`namespace_id`）。
3. `group_ids` の各グループ（と `project_ids` の各プロジェクト）に、有効な利用者を Owner として加える。メンバーを変える操作の手段（HTTP の経路）ができるまでは実行できない:
   `group_ids` を障害記録に残し、手段ができたら加える。それまで `forge doctor` は FG-AUTH-0010（`reason=ownerless_group`）を出し続ける（warn）。
   手で `forge.group_members` に INSERT しない（監査のトリガーが拒み、監査の行を手で書くと封印の連鎖を壊しうる）。

## Verify

- `forge doctor` の `users.administration` が `ok`（有効な人間の管理者がいて、どの最上位のグループにも Owner がいる）。
- 無効化した利用者のトークン: `forge admin token list --user <username>` で、すべてに `revoked_at` がある。
- 監査の記録に、実行した手順ごとに 1 行ある（`user.disabled`、`user.admin_bootstrapped`）。

## Escalate

- 強制した無効化の記録がないのに FG-AUTH-0010 が出る: データが forge-db の外で変えられた。`/incident` で事案にし、DB の管理者に連絡する。
- 漏えいの対応で無効化した: 事案として記録し（`/incident`）、失効したトークンの `public_id` で `forge.audit_log` とログを調べる。
- 事後: 管理者を常に 2 人以上置く運用にする（最後の管理者の拒否と緊急の `--force` を避けるため）。
