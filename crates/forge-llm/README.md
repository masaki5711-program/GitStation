# forge-llm

**Responsibility**: the local LLM gateway (plan 19 L5-3, ADR-0039). One
`Gateway` per `forge worker` asks the in-house Ollama — shared with Kanflow —
for advice: MR first pass, incident first diagnosis, approval summary (the
features land in L5-5+). The LLM decides nothing; a failure is never reported
as success.

A feature crate: `forge-cli → forge-llm → forge-db → forge-core → forge-errors`.
No SQL of its own (`forge_db::llm`); no other feature crate. The contract
(purposes, priorities, input classes, secret redaction, output checks, the
checked retry, `ModelRef`) is pure and lives in `forge_core::llm`.

**Public API**
- `Gateway::new(GatewayConfig, Arc<dyn LlmProvider>, Arc<dyn Records>)`,
  `submit(Submission) -> Outcome` (`Advised` / `Paused`), `pause` / `resume`,
  `verify_model(&dyn ModelCatalog)`, `set_conditions`, `tick`, `metrics()`.
- `ollama::OllamaProvider` (`/api/chat`, `/api/tags`, `/api/ps` for doctor; rustls with `llm.ca_file`
  only, no proxy, no redirect, a timeout on every call).
- `kanflow::KanflowGate` (Kanflow's `GET /api/internal/ai-busy`), `NoGate`.
- `sched::Scheduler` (pure): concurrency, 10-minute promotion, the 20 % review
  reserve, Kanflow busy / unreachable (1 per minute), `gpu-eval` holds batch.
- `background::spawn`: clock, Kanflow, `gpu-eval`, model digest (10 min),
  metrics snapshot (5 min).
- `records::DbRecords` (`forge.llm_requests` / `forge.llm_advice`).
- `prompts::mr_review()` (`prompts/mr_review.v1.txt`, L5-5: the merge
  request first pass, `forge/ai-review`) and `Submission::deep_review` (the
  code rules' deep-review reason, stored before the model's,
  `forge_llm::deep_review`).
- `prompts::approval_summary()` (`prompts/approval_summary.v1.txt`, L5-8): the
  versioned prompts, compiled in; a new text is a new file and version.
  `prompts::for_purpose` is the one list of prompts and output policies in
  use (the worker asks with them, the bench measures them).
- `bench` and the bin `llm-bench` (L5-10): the synthetic eval sets
  `evals/<purpose>/*.toml` and `evals/bench.toml` run through `prepare` →
  prompt → `generate_checked`, scored into the ADR-0034 `eval-report.json`
  (`.forge-ci.yml` job `llm-eval`, tag `gpu-eval`). By hand on the
  development server: `cargo run -p forge-llm --bin llm-bench --
  --endpoint http://127.0.0.1:11434 --out eval-report.json`. Exit 1 with
  `error_code=…` and no report when the model is unreachable (FG-LLM-0001),
  not the pinned digest (FG-LLM-0003) or the inputs are invalid
  (FG-SYS-0002 `config_key=llm_bench.*`).

**Owned error codes**: FG-LLM-0001 LlmUnavailable, FG-LLM-0003
LlmModelDigestMismatch (alert), FG-LLM-0004 LlmQueueDeadline; it logs
FG-LLM-0002 / 0006 from `forge_core::llm` at its boundary. The gateway logs
each failure once (with `request_id`, `llm_request_id`, `purpose`, `model`,
`prompt_version`); callers act on the returned code without logging it again.

**Runbooks**: `ops/runbooks/llm-gateway.md`, `ops/runbooks/llm-model-update.md`,
`ops/runbooks/llm-advice-quality.md` (L5-11). `forge doctor` (forge-cli
`doctor/llm.rs`): `llm.endpoint`, `llm.model`, `llm.num_ctx` (`OllamaProvider::loaded`,
`/api/ps`), `llm.kanflow_gate` (`KanflowGate::probe`), `llm.queue_age`
(`forge_db::llm::queue_health`).

**Configuration**: `[llm]` (`forge_core::config::llm`), disabled by default.

**Tests** never call a real model: a scripted provider, in-memory records and a
local fake HTTP server.
