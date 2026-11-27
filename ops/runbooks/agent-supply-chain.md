# Runbook: agent-supply-chain（FG-AGENT-0002、FG-AGENT-0016。関連: FG-AGENT-0003、FG-AGENT-0005、FG-AGENT-0006、FG-AGENT-0007、FG-AGENT-0013）

計画 16（ADR-0032）のエージェントの部品の署名か要約が合わないとき。リポジトリの `.claude/`・`CLAUDE.md`・`.mcp.json` は開発者の PC で
`node` として動くので、**改ざんの疑いとして扱う**（SEV2 から始め、影響の範囲が分かってから下げる）。

A1 の時点では、確かめるのは開発者の PC の `gstn agent verify` だけ（サーバーの公開・`forge/agent-lock`・取り消しは A5・A7 で足す。そのときこの
Runbook に手順を足す）。

## Trigger

- `gstn agent verify` が `FG-AGENT-0002` PartSignatureInvalid で終わった（終了コード 1）。`reason`:
  - `digest_mismatch`: lock の欄（ファイルの SHA-256 など）から作り直した要約が、lock の `digest` と違う。lock とファイルが一緒に書き換えられた。
  - `bad_signature`: 要約まで作り直されたか、署名・鍵が違う。
  - `signature_malformed`: 署名が base64 の 64 バイトでない。
  - `key_revoked`: 署名した鍵が失効している（鍵の盗難の対応の後に出る）。
- 開発者から「`gstn agent verify` が通らない」と問い合わせがあった。
- （A9）`FG-AGENT-0016` AgentUpdateNotOpened: 部品の版を取り消した後、そのプロジェクトの更新の MR を開けなかった（ジョブ
  `agent.update_merge_request` が `dead`）。`reason`: `no_author`（人が頼んだビルドが無い）、`author_not_allowed`（`denied_code`・`candidates`:
  ビルドを頼んだ人（新しい順に最大 5 人）がみな抜けた・無効になった・Developer でない・`agent-builder/*` が保護されている）、`closed_by_person`（同じ更新の MR を
  人が閉じた）。取り消した版がそのプロジェクトの lock に残り、`forge/agent-lock` は FG-AGENT-0006 で失敗し続ける。

## Impact

- その作業ツリーで Claude Code を動かすと、公開されていないフックや規則が動く恐れがある。同じ版の lock を持つ全リポジトリ・全開発者が対象になりうる。
- 1 人の作業ツリーだけなら SEV3。`main` にある lock・ファイルで起きるなら SEV2（全員の PC）。

## Diagnose（読み取りのみ）

1. 開発者に出力を JSON でもらう: `gstn agent verify --json`。`problems[].subject` に `slug@version key <kid>`、`reason` がある。
2. どこが変わったか（開発者の作業ツリーで、読むだけ）:
   - `git status -- .claude CLAUDE.md .mcp.json .gitstation` — まだコミットしていない変更か。
   - `git log -p -- .gitstation/agent.lock.json .claude` — 誰が、どのコミット・MR で lock とファイルを変えたか。ビルダーの MR（A7 以降はボットの
     コミット）以外の変更が疑わしい。
   - `main` で起きるか: 新しい clone か `git worktree add` した `main` で `gstn agent verify`。
3. `key_revoked` なら、鍵の失効の記録（A3 以降は `forge.agent_signing_keys.revoked_at`）と失効の理由を管理者に確かめる。
4. 同じ `slug@version` を使う他のリポジトリ（A5 以降は `gstn part view <slug>` の利用先）。
5. （A9）取り消しの後の更新の MR（読み取りのプール）:
   `SELECT id, state, attempts, last_error_code, payload FROM forge.jobs WHERE kind = 'agent.update_merge_request' ORDER BY id DESC`
   — 取り消しのトランザクションが、その版を選んだビルドのあるプロジェクトごとに 1 つ入れる（dedup `agent_update_revoked:<version_id>:<project_id>`）。
   `succeeded` のジョブはログの `forge.agent.update_opened`（`iid`）か `forge.agent.update_skipped`（`reason`: `not_in_lock` = もう誰かが更新した、
   `up_to_date`、`no_lock`）を持つ。`dead` は `last_error_code`（FG-AGENT-0016 なら `reason`）を見る。

## Mitigate（変更は人の承認のあと）

1. **その作業ツリーで Claude Code を動かさない**ように開発者に伝える（既に動かしたなら、その PC を調べる対象にする）。
2. 作業ツリーを最後に確かめた状態へ戻す: `git restore --source <確かめたコミット> -- .claude CLAUDE.md .mcp.json .gitstation` の後、
   `gstn agent verify` が通ることを確かめる。`main` に入っているなら、戻す MR を作り、人の Maintainer が承認する（自動マージにしない。ADR-0017 Amendment
   2026-11-12 の `agent_config`）。
3. 変えた人とその MR を記録する。意図した変更（手で足した規則など）なら、ビルダーの MR で `custom` として入れ直す。
4. `key_revoked` なら、部品を新しい鍵で公開し直し、使っているリポジトリを更新の MR で直す（A5・A9。鍵は Runbook `agent-signing-key`）。
5. **疑いの版を取り消す**（画面 `/-/marketplace/<slug>`、公開のグループの Maintainer か管理者、人だけ。取り消しは最終）。A9 以降、取り消しは
   その版を使う各プロジェクトに更新の MR を 1 回だけ開く（ビルドを頼んだ人のうち新しい順に最初に許される人の名前で、`agent-builder/<lock の要約>` の枝。自動マージは
   しない: 作成者ではない Maintainer の承認が要る）。置き換えの stable の版が無ければ、その部品は MR で外れる。
6. （A9）`FG-AGENT-0016` のプロジェクトは、そのプロジェクトの Developer 以上の人に **更新を頼んでもらう**
   （`POST /api/v1/projects/{id}/agent-builder/update`。その人の名前で開く）。`closed_by_person` は閉じた理由を確かめ、それでも更新が要るなら
   枝 `agent-builder/<digest>` を消してから頼む。開発者には、マージまで `gstn agent verify` が通らない作業ツリーで Claude Code を動かさないよう伝える。

## Verify

- 開発者の作業ツリーと `main` の両方で `gstn agent verify` が終了コード 0、`problems: []`。
- 原因の変更（コミット・MR）が特定され、インシデントの記録に書かれている。
- （A9）取り消した版を使っていた各プロジェクトで、更新の MR がマージされ、`forge/agent-lock` が `main` で緑。`agent.update_merge_request` の
  `dead` のジョブが残っていない（残るものは 6 の手で頼んだ更新で置き換わっている）。

## 関連

- ADR-0032（形・要約・署名）、計画 16 §8（危険と対策）、Runbook `agent-signing-key`。
- FG-AGENT-0007（ファイルだけが lock と違う: 手で編集）、FG-AGENT-0005（宣言していない能力）。
