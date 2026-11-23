// Reads the summary of the `forge/ai-eval` check run (ADR-0034 decision 7,
// written by forge_core::ai_eval::Evaluation::summary and, for `error`, by
// forge-pipelines) back into rows for the comparison table on the merge
// request (plan 17 P17-9). The shape is shared with the Rust test
// `summary_matches_the_web_golden` through tests/fixtures/
// ai-eval-summary.golden.md: a change there fails that test first.
// Anything that does not read returns null, and the screen shows the
// summary as written instead (never an empty table).

export const AI_EVAL_CHECK = 'forge/ai-eval';

export type AiEvalCaseResult = 'blocking' | 'flaky' | 'fixed' | 'fail' | 'pass';
export type AiEvalCaseKind = 'regular' | 'safety' | 'reproduction';
export type AiEvalMetricResult = 'ok' | 'past_delta' | 'shown';

export interface RunCount {
  passes: number;
  runs: number;
}

export interface AiEvalMetric {
  name: string;
  baseline: number;
  now: number;
  /** The allowed delta (absolute, in the metric's unit); null: shown only. */
  allowed: number | null;
  result: AiEvalMetricResult;
}

export interface AiEvalFinding {
  code: string;
  reason: string;
  /** The case or metric the finding is about. */
  subject: string | null;
  /** The line as written (English), for a reason this screen does not know. */
  text: string;
}

export interface AiEvalCase {
  id: string;
  kind: AiEvalCaseKind;
  baseline: RunCount | null;
  now: RunCount;
  result: AiEvalCaseResult;
}

export interface AiEvalEvaluated {
  kind: 'evaluated';
  passed: boolean;
  models: string;
  total: number;
  safety: number;
  reproduction: number;
  flaky: number;
  metrics: AiEvalMetric[];
  findings: AiEvalFinding[];
  /** Blocking first, then flaky, fixed, failing, passing (as written). */
  cases: AiEvalCase[];
  /** Cases left out of the summary (all of them are in eval-report.json). */
  more: number;
  /** The findings list was cut short. */
  findingsCut: boolean;
}

export interface AiEvalError {
  kind: 'error';
  code: string;
  reason: string;
  pipelineId: number | null;
  job: string | null;
  runbook: string | null;
}

export type AiEvalReport = AiEvalEvaluated | AiEvalError;

const CASE_RESULTS: readonly string[] = ['blocking', 'flaky', 'fixed', 'fail', 'pass'];
const CASE_KINDS: readonly string[] = ['regular', 'safety', 'reproduction'];
const METRIC_RESULTS: Record<string, AiEvalMetricResult> = { ok: 'ok', 'past delta': 'past_delta', shown: 'shown' };

const HEAD =
  /^\*\*forge\/ai-eval (passed|failed)\*\* — models `([^`]+)`, (\d+) cases \((\d+) safety, (\d+) reproduction\); (\d+) flaky \(listed, not counted\); (\d+) findings\.$/;
const ERROR_HEAD = /^\*\*forge\/ai-eval error\*\* \[(FG-[A-Z]+-\d{4})\] ([a-z0-9_]+) — pipeline #(\d+), job `([^`]*)`\./;
const RUNBOOK = / Runbook: ([A-Za-z0-9_-]+)\.\s*$/;
const FINDING = /^- \[(FG-[A-Z]+-\d{4})\] ([a-z0-9_]+)(?: `([^`]+)`)?/;
const MORE = /^… (\d+) more cases/;

function num(s: string | undefined): number | null {
  if (s === undefined) return null;
  const n = Number(s.trim());
  return Number.isFinite(n) && s.trim() !== '' ? n : null;
}

function count(s: string | undefined): RunCount | null {
  const m = /^(\d+)\/(\d+)$/.exec((s ?? '').trim());
  if (m === null) return null;
  return { passes: Number(m[1]), runs: Number(m[2]) };
}

/** The cells of a `| a | b |` row; null when the line is no table row. */
function cells(line: string): string[] | null {
  const t = line.trim();
  if (!t.startsWith('|') || !t.endsWith('|')) return null;
  return t
    .slice(1, -1)
    .split('|')
    .map((c) => c.trim());
}

const unquote = (s: string) => (s.startsWith('`') && s.endsWith('`') && s.length >= 2 ? s.slice(1, -1) : s);

function parseError(summary: string): AiEvalError | null {
  const m = ERROR_HEAD.exec(summary);
  if (m === null) return null;
  const pipeline = Number(m[3]);
  return {
    kind: 'error',
    code: m[1] ?? '',
    reason: m[2] ?? '',
    pipelineId: Number.isSafeInteger(pipeline) && pipeline > 0 ? pipeline : null,
    job: m[4] === undefined || m[4] === '' ? null : m[4],
    runbook: RUNBOOK.exec(summary)?.[1] ?? null,
  };
}

/** The report in the summary of a `forge/ai-eval` run; null if it does not read. */
export function parseAiEvalSummary(summary: string): AiEvalReport | null {
  const text = summary.replace(/\r\n/g, '\n');
  if (text.startsWith('**forge/ai-eval error**')) return parseError(text);
  const lines = text.split('\n');
  const head = HEAD.exec(lines[0] ?? '');
  if (head === null) return null;
  const report: AiEvalEvaluated = {
    kind: 'evaluated',
    passed: head[1] === 'passed',
    models: head[2] ?? '',
    total: Number(head[3]),
    safety: Number(head[4]),
    reproduction: Number(head[5]),
    flaky: Number(head[6]),
    metrics: [],
    findings: [],
    cases: [],
    more: 0,
    findingsCut: false,
  };
  let table: 'metric' | 'case' | null = null;
  for (const line of lines.slice(1)) {
    const row = cells(line);
    if (row !== null) {
      if (row[0] === 'Metric') table = 'metric';
      else if (row[0] === 'Case') table = 'case';
      else if (row.every((c) => /^-+$/.test(c))) continue;
      else if (table === 'metric') {
        const metric = parseMetric(row);
        if (metric === null) return null;
        report.metrics.push(metric);
      } else if (table === 'case') {
        const c = parseCase(row);
        if (c === null) return null;
        report.cases.push(c);
      } else return null;
      continue;
    }
    table = null;
    if (line.startsWith('- … ')) {
      report.findingsCut = true;
      continue;
    }
    const finding = FINDING.exec(line);
    if (finding !== null) {
      report.findings.push({
        code: finding[1] ?? '',
        reason: finding[2] ?? '',
        subject: finding[3] ?? null,
        text: line.slice(2),
      });
      continue;
    }
    const more = MORE.exec(line);
    if (more !== null) report.more = Number(more[1]);
  }
  // A table that did not read at all is no comparison: say so, not "nothing".
  if (report.metrics.length === 0 || (report.cases.length === 0 && report.total > 0)) return null;
  return report;
}

function parseMetric(row: string[]): AiEvalMetric | null {
  if (row.length !== 5) return null;
  const [name, b, n, allowed, result] = row;
  const baseline = num(b);
  const now = num(n);
  const res = METRIC_RESULTS[result ?? ''];
  if (name === undefined || name === '' || baseline === null || now === null || res === undefined) return null;
  const a = allowed === '—' ? null : num(allowed);
  if (a === null && allowed !== '—') return null;
  return { name, baseline, now, allowed: a, result: res };
}

function parseCase(row: string[]): AiEvalCase | null {
  if (row.length !== 5) return null;
  const [id, kind, b, n, result] = row;
  const now = count(n);
  const baseline = b === '—' ? null : count(b);
  if (id === undefined || kind === undefined || result === undefined || now === null) return null;
  if (baseline === null && b !== '—') return null;
  if (!CASE_KINDS.includes(kind) || !CASE_RESULTS.includes(result)) return null;
  return {
    id: unquote(id),
    kind: kind as AiEvalCaseKind,
    baseline,
    now,
    result: result as AiEvalCaseResult,
  };
}

/** Metrics that are rates (0–1), shown in percent; the rest are milliseconds. */
export const isRate = (name: string) => name !== 'p95_latency_ms';

/** Which way is worse for a gated metric: a drop (hit rate) or a rise. */
export const worseWhenLower = (name: string) => name === 'cause_hit_rate';
