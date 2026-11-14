# Runbook: agent-supply-chain（FG-AGENT-0002。関連: FG-AGENT-0003、FG-AGENT-0005、FG-AGENT-0007、FG-AGENT-0013）

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

## Mitigate（変更は人の承認のあと）

1. **その作業ツリーで Claude Code を動かさない**ように開発者に伝える（既に動かしたなら、その PC を調べる対象にする）。
2. 作業ツリーを最後に確かめた状態へ戻す: `git restore --source <確かめたコミット> -- .claude CLAUDE.md .mcp.json .gitstation` の後、
   `gstn agent verify` が通ることを確かめる。`main` に入っているなら、戻す MR を作り、人の Maintainer が承認する（自動マージにしない。ADR-0017 Amendment
   2026-11-12 の `agent_config`）。
3. 変えた人とその MR を記録する。意図した変更（手で足した規則など）なら、ビルダーの MR で `custom` として入れ直す。
4. `key_revoked` なら、部品を新しい鍵で公開し直し、使っているリポジトリを更新の MR で直す（A5・A9。鍵は Runbook `agent-signing-key`）。

## Verify

- 開発者の作業ツリーと `main` の両方で `gstn agent verify` が終了コード 0、`problems: []`。
- 原因の変更（コミット・MR）が特定され、インシデントの記録に書かれている。

## 関連

- ADR-0032（形・要約・署名）、計画 16 §8（危険と対策）、Runbook `agent-signing-key`。
- FG-AGENT-0007（ファイルだけが lock と違う: 手で編集）、FG-AGENT-0005（宣言していない能力）。
