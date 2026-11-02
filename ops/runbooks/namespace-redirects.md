# Runbook: namespace-redirects（`namespace.redirected`、FG-REPO-0018、FG-REPO-0022。関連: FG-REPO-0009、FG-REPO-0015、FG-DB-0002）

プロジェクト・グループの移動や名前の変更、ユーザーの名前の変更（手順 D5 MR5、ADR-0015 の Amendment D5）は、動いた名前空間と
その子孫のすべてについて、古いパスに `moved` の墓標を 1 行ずつ書く。墓標は解放されるまで古いパスを塞ぎ、次のように働く。

- **git の clone / fetch**: 古いパスへの `info/refs?service=git-upload-pack` は、`authorize(RepoRead)` で許可された呼び出しだけ
  `302`（`Cache-Control: no-store`）で新しいパスへ送る。許可されない呼び出しには、存在しないのと同じ 404（`FG-REPO-0005`）。
  upload-pack の POST は古いパスでもそのまま処理する（資格情報の付いた再試行では git が古いパスへ POST するため）。
- **push は送らない**: 古いパスへの receive-pack は 404（`reason=tombstone_push`）。利用者が remote を直すまで失敗する。
- **作成・移動**: 古いパスへの作成や移動は 409（`FG-REPO-0009 reason=tombstoned`）。墓標を解放するまで使えない。自分の古いパスへ
  戻すときは `reclaim`（API の `"reclaim": true`、CLI の `--reclaim`）で、同じ名前空間を指す墓標だけを同じ変更で解放できる。

墓標を通った要求は、warn のログ `namespace.redirected`（`namespace_id`、`tombstone_id`、`route`、`outcome` = `redirect` / `served`）
になる。パスや利用者名はログに出さない。

## Trigger

- `namespace.redirected` の件数が多い、または減らない（古い URL を使い続けているクライアントや CI がある）。
- 利用者から「push が 404 になる」（古い remote のまま）、「名前を戻せない」（409 `tombstoned`）、「移動できない」
  （429 `FG-REPO-0018`、503 `FG-REPO-0022`）の問い合わせ。

## Diagnosis（読み取りだけ）

1. **どの名前空間への転送か**: ログの `namespace_id` / `tombstone_id` を数える（`request_id` で個々の要求を追える）。
   権限のあるクエリで `SELECT id, old_path, kind, namespace_id, created_at FROM forge.namespace_tombstones WHERE id = <tombstone_id>`。
   今のパスは `SELECT full_path FROM forge.namespaces WHERE id = <namespace_id>`。
2. **移動の記録**: `SELECT occurred_at, actor_kind, actor_label, actor_user_id, action, details FROM forge.audit_log
   WHERE action IN ('project.moved', 'group.moved', 'user.renamed') AND resource_type = '<project|group|user>' AND resource_id = <id>
   ORDER BY occurred_at DESC`。祖先の移動で書かれたプロジェクトごとの行は `details ? 'moved_namespace_id'`。
3. **429 `FG-REPO-0018`**: 上の 2 の行のうち、祖先の移動の行を除いた直近 24 時間の件数が 10 以上。最も古いものが 24 時間を過ぎれば
   また移動できる。
4. **503 `FG-REPO-0022`**: ログの `command`、`attempts`、`last_reason`（`deadlock` / `subtree_unstable` / `tombstone_changed` /
   `serialization`）。同じ時刻の同じ部分木の移動・作成・完全削除（`forge.audit_log` を `occurred_at` で）、`pg_stat_activity` /
   `pg_locks` の長いトランザクションを見る。何も書かれていない（再試行してよい）。

## Mitigation

- **古い URL の利用者に知らせる**（推奨）: 1 のパスの所有者に、remote の更新（`git remote set-url origin <新しい URL>`）と、CI・
  ドキュメントの URL の更新を依頼する。clone / fetch は転送で動き続けるが、push は直すまで失敗する。
- **墓標の解放**（パスをもう一度使えるようにする。転送も終わる）: 行き先の今の Owner か管理者が
  `POST /api/v1/namespaces/tombstones/release`（本文 `{"path": "<古いパス>"}`）。削除の墓標は、記録した Owner が親の名前空間で今も
  Owner のときだけその人が、それ以外は管理者だけが解放できる。解放は監査に `namespace_tombstone.released` として残る。
- **回数の上限を超えた移動を、承認を得て行う**（運用の CLI は上限の対象外。本番では承認済みの手順として実行する）:
  - `forge admin project move <project_id> --namespace <親の namespace_id> --path <名前> [--reclaim]`
  - `forge admin group move <group_id> (--namespace <親の namespace_id> | --top-level) --path <名前> [--reclaim]`
  - `forge admin user rename <今のユーザー名> <新しいユーザー名> [--reclaim]`

  API と同じ規則（部分木の上限 1000、深さ 20、パス 255 文字、循環、移動先の種類、衝突、Owner がいなくならないこと）で
  `system` として実行し、監査の行に `actor_label = forge-cli:<OS のアカウント>` が残る。出力は 1 行（`kind id old_path new_path
  tombstones projects`）。
- `FG-REPO-0022` が続くとき: 競合している書き手（多数の名前空間を一度に動かすスクリプト、部分木の完全削除）を先に終わらせ、
  大きなグループは分けて動かす。

## Verify

- 移動の後: 新しいパスで `GET /api/v1/namespaces/resolve?path=<新しいパス>` が 200、古いパスの `info/refs?service=git-upload-pack`
  が読める利用者に 302、`git-receive-pack` が 404。
- 解放の後: 古いパスの resolve が 404、そのパスへの作成が成功する。
- `namespace.redirected` の件数が下がっていく。

## Escalation

- 墓標が書かれていない移動（`moved` の墓標の数が監査の `details.nodes` と合わない）や、古いパスと今のパスが同時に存在する
  （`SELECT count(*) FROM forge.namespaces n JOIN forge.namespace_tombstones t ON t.old_path = n.full_path` が 0 でない）は不具合。
  runbook `namespace-data` に進み、書き込みを止めて調べる。
