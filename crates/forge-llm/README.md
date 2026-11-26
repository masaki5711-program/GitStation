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
- `ollama::OllamaProvider` (`/api/chat`, `/api/tags`; rustls with `llm.ca_file`
  only, no proxy, no redirect, a timeout on every call).
- `kanflow::KanflowGate` (Kanflow's `GET /api/internal/ai-busy`), `NoGate`.
- `sched::Scheduler` (pure): concurrency, 10-minute promotion, the 20 % review
  reserve, Kanflow busy / unreachable (1 per minute), `gpu-eval` holds batch.
- `background::spawn`: clock, Kanflow, `gpu-eval`, model digest (10 min),
  metrics snapshot (5 min).
- `records::DbRecords` (`forge.llm_requests` / `forge.llm_advice`).
- `prompts::approval_summary()` (`prompts/approval_summary.v1.txt`, L5-8): the
  versioned prompts, compiled in; a new text is a new file and version.

**Owned error codes**: FG-LLM-0001 LlmUnavailable, FG-LLM-0003
LlmModelDigestMismatch (alert), FG-LLM-0004 LlmQueueDeadline; it logs
FG-LLM-0002 / 0006 from `forge_core::llm` at its boundary. The gateway logs
each failure once (with `request_id`, `llm_request_id`, `purpose`, `model`,
`prompt_version`); callers act on the returned code without logging it again.

**Runbooks**: `ops/runbooks/llm-gateway.md`, `ops/runbooks/llm-model-update.md`.

**Configuration**: `[llm]` (`forge_core::config::llm`), disabled by default.

**Tests** never call a real model: a scripted provider, in-memory records and a
local fake HTTP server.
