// The AI evaluation on the merge request (plan 17 P17-9): the summary of the
// `forge/ai-eval` check (ADR-0034) read back into a comparison table in
// plain words. One line first (why it stops the merge, what to do next);
// every result is a word, never a colour alone; a summary that does not
// read is shown as written, never as an empty table. The golden summary is
// the one the Rust test `summary_matches_the_web_golden` checks against
// forge_core::ai_eval, so both sides read the same shape. Synthetic data.
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

import axe from 'axe-core';
import { fireEvent, screen, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { UNREADABLE_SHOWN_MAX } from '../src/features/mr/AiEvalReport';
import { parseAiEvalSummary, SUMMARY_READ_MAX, type AiEvalEvaluated } from '../src/features/mr/aiEval';
import { mockServer } from './mock';
import { CHECKS_API, MR_SCREENS, checkRun, checkRuns, mrRoutes } from './mr-fixtures';
import { json, renderApp } from './render';

const GOLDEN = readFileSync(join(import.meta.dirname, 'fixtures/ai-eval-summary.golden.md'), 'utf8');
const VIEW = `${MR_SCREENS}/pull/3`;

const ERROR_SUMMARY =
  '**forge/ai-eval error** [FG-AIASSET-0002] report_missing — pipeline #12, job `bench`. At fault: eval-report.json. ' +
  'The AI evaluation bench produced no result. `forge/ai-eval` is reported as error. Runbook: ai-eval-bench.';

const PASSED_SUMMARY = `**forge/ai-eval passed** — models \`qwen3-14b\`, 1 cases (0 safety, 0 reproduction); 0 flaky (listed, not counted); 0 findings.

| Metric | Baseline | Now | Allowed | Result |
|---|---|---|---|---|
| cause_hit_rate | 0.800 | 0.810 | 0.020 | ok |

| Case | Kind | Baseline | Now | Result |
|---|---|---|---|---|
| \`spindle/a-001\` | regular | 5/5 | 5/5 | pass |
`;

const PIPELINE_RUN = checkRun(8, 'ci/pipeline', { summary: 'Pipeline #31 failed: 1 of 2 jobs did not succeed.' });

const aiEval = (over: Record<string, unknown>) => checkRun(9, 'forge/ai-eval', { reported_by: null, ...over });

async function open(runs: unknown[], lang: 'en' | 'ja' = 'en') {
  const server = mockServer(mrRoutes({ [`GET ${CHECKS_API}`]: () => json(200, checkRuns(runs)) }));
  const view = await renderApp({ fetch: server.fetch, path: VIEW, lang });
  await screen.findByRole('heading', { level: 1, name: /Synthetic change 3/ });
  return view;
}

const section = async (name: string | RegExp) =>
  (await screen.findByRole('heading', { level: 2, name })).closest('section') as HTMLElement;

describe('ai-eval summary parser', () => {
  it('reads the golden summary forge_core writes: verdict, counts, metrics, findings, cases', () => {
    const r = parseAiEvalSummary(GOLDEN) as AiEvalEvaluated;
    expect(r.kind).toBe('evaluated');
    expect(r).toMatchObject({ passed: false, models: 'qwen3-14b', total: 6, safety: 1, reproduction: 1, flaky: 3, more: 0 });
    expect(r.metrics.map((m) => [m.name, m.result])).toEqual([
      ['cause_hit_rate', 'past_delta'],
      ['false_alarm_rate', 'ok'],
      ['p95_latency_ms', 'ok'],
      ['attach_precision', 'shown'],
      ['abstain_accuracy', 'shown'],
    ]);
    expect(r.metrics[0]).toMatchObject({ baseline: 0.8, now: 0.75, allowed: 0.02 });
    expect(r.metrics[3]?.allowed).toBeNull();
    expect(r.findings.map((f) => [f.code, f.reason, f.subject])).toEqual([
      ['FG-AIASSET-0004', 'safety_case_failed', 'safety/s-001'],
      ['FG-AIASSET-0004', 'stable_case_regressed', 'spindle/a-001'],
      ['FG-AIASSET-0004', 'metric_below_baseline', 'cause_hit_rate'],
    ]);
    expect(r.cases.map((c) => [c.id, c.kind, c.result])).toEqual([
      ['safety/s-001', 'safety', 'blocking'],
      ['spindle/a-001', 'regular', 'blocking'],
      ['bearing/b-001', 'regular', 'flaky'],
      ['repro/r-001', 'reproduction', 'fixed'],
      ['new/n-001', 'regular', 'fail'],
      ['spindle/a-002', 'regular', 'pass'],
    ]);
    expect(r.cases[4]?.baseline).toBeNull();
    expect(r.cases[0]).toMatchObject({ baseline: { passes: 5, runs: 5 }, now: { passes: 4, runs: 5 } });
  });

  it('reads an error summary: code, reason, pipeline, job, runbook', () => {
    expect(parseAiEvalSummary(ERROR_SUMMARY)).toEqual({
      kind: 'error',
      code: 'FG-AIASSET-0002',
      reason: 'report_missing',
      pipelineId: 12,
      job: 'bench',
      runbook: 'ai-eval-bench',
    });
  });

  it('counts the cases left out and a cut findings list', () => {
    const cut = GOLDEN.replace('- [FG-AIASSET-0004] metric_below', '- … (more in eval-report.json)\n- [FG-AIASSET-0004] metric_below') +
      '\n… 42 more cases (all of them are in eval-report.json).\n';
    const r = parseAiEvalSummary(cut) as AiEvalEvaluated;
    expect(r.more).toBe(42);
    expect(r.findingsCut).toBe(true);
  });

  it('reads CRLF line ends and a metric with an allowed difference', () => {
    const r = parseAiEvalSummary(GOLDEN.replace(/\n/g, '\r\n')) as AiEvalEvaluated;
    expect(r.cases).toHaveLength(6);
    expect(r.metrics.find((m) => m.name === 'p95_latency_ms')?.allowed).toBe(8000);
  });

  it('an error summary without a runbook or a pipeline id still reads', () => {
    const r = parseAiEvalSummary(
      '**forge/ai-eval error** [FG-AIASSET-0003] baseline_missing — pipeline #0, job ``. No baseline.',
    );
    expect(r).toEqual({ kind: 'error', code: 'FG-AIASSET-0003', reason: 'baseline_missing', pipelineId: null, job: null, runbook: null });
  });

  it('refuses a column-count mismatch, a row before its header, duplicates, odd numbers and a huge input', () => {
    expect(parseAiEvalSummary(GOLDEN.replace('| `new/n-001` | regular | — | 0/5 | fail |', '| `new/n-001` | regular | 0/5 | fail |'))).toBeNull();
    const head = GOLDEN.split('\n')[0] ?? '';
    expect(parseAiEvalSummary(`${head}\n| cause_hit_rate | 0.800 | 0.750 | 0.020 | ok |\n`)).toBeNull();
    expect(parseAiEvalSummary(GOLDEN.replace('| `spindle/a-002` |', '| `spindle/a-001` |'))).toBeNull();
    expect(parseAiEvalSummary(GOLDEN.replace('| false_alarm_rate |', '| cause_hit_rate |'))).toBeNull();
    for (const odd of ['0x10', '1e3', 'Infinity', ' ', '.5']) {
      expect(parseAiEvalSummary(GOLDEN.replace('| 0.750 |', `| ${odd} |`))).toBeNull();
    }
    expect(parseAiEvalSummary(GOLDEN + 'x'.repeat(SUMMARY_READ_MAX))).toBeNull();
  });

  it('anything that does not read is null, never an empty comparison', () => {
    expect(parseAiEvalSummary('')).toBeNull();
    expect(parseAiEvalSummary('Pipeline #3 passed')).toBeNull();
    // The head reads, the tables do not.
    expect(parseAiEvalSummary(GOLDEN.split('\n')[0] ?? '')).toBeNull();
    // A row with an unknown result.
    expect(parseAiEvalSummary(GOLDEN.replace('| 4/5 | blocking |', '| 4/5 | maybe |'))).toBeNull();
    expect(parseAiEvalSummary(GOLDEN.replace('| 0.750 |', '| n/a |'))).toBeNull();
    expect(parseAiEvalSummary('**forge/ai-eval error** something else')).toBeNull();
  });
});

describe('merge request AI evaluation', () => {
  it('a failed evaluation: one line with why and what next, the reasons, the measures and the cases in words', async () => {
    await open([aiEval({ conclusion: 'failure', summary: GOLDEN }), PIPELINE_RUN]);
    const box = await section('AI evaluation (compared with the approved baseline)');
    const line = within(box).getByTestId('ai-eval-line');
    expect(line.textContent).toContain(
      'worse than the baseline in 3 points, so the merge is stopped. Next: fix those cases and push a new version.',
    );
    // The line names where to act: the reasons, and the pipeline of the head.
    expect(within(line).getByRole('link', { name: 'See what stops the merge' }).getAttribute('href')).toBe(
      '#mr-ai-eval-findings',
    );
    expect(within(line).getByRole('link', { name: 'Open pipeline #31' }).getAttribute('href')).toMatch(/pipelines\/31$/);
    // Wide tables scroll inside a focusable, named region (375 px).
    const region = within(box).getByRole('region', { name: 'Cases to look at' });
    expect(region.getAttribute('tabindex')).toBe('0');
    expect(within(box).getByText(/Model qwen3-14b · 6 cases \(1 safety, 1 reproducing a miss\)/)).toBeTruthy();
    expect(within(box).getByText(/3 cases whose answers changed from run to run are not counted/)).toBeTruthy();
    // The reasons, in plain words, with their code.
    within(box).getByRole('heading', { level: 3, name: 'What stops the merge' });
    expect(within(box).getByText(/Safety case safety\/s-001 did not pass every run/)).toBeTruthy();
    expect(within(box).getByText(/Case spindle\/a-001 passed every run in the baseline and now fails/)).toBeTruthy();
    expect(within(box).getByText(/The measure “Correct cause” is worse than the baseline/)).toBeTruthy();
    // The measures: percent, difference, allowed difference, a word for the result.
    const metrics = within(box).getByRole('table', { name: 'Measures compared with the baseline' });
    const hit = within(metrics).getByRole('row', { name: /^Correct cause/ });
    expect(within(hit).getByText('80.0%')).toBeTruthy();
    expect(within(hit).getByText('75.0%')).toBeTruthy();
    expect(within(hit).getByText('-5.0 pt')).toBeTruthy();
    expect(within(hit).getByText('up to -2.0 pt')).toBeTruthy();
    expect(within(hit).getByText(/Out of range/)).toBeTruthy();
    const p95 = within(box).getByRole('row', { name: /^Response time/ });
    expect(within(p95).getByText('40.0 s')).toBeTruthy();
    expect(within(p95).getByText('+1.0 s')).toBeTruthy();
    expect(within(p95).getByText('up to +8.0 s')).toBeTruthy();
    expect(within(p95).getByText(/Within range/)).toBeTruthy();
    expect(within(within(box).getByRole('row', { name: /^Correct attachments/ })).getByText('For reference')).toBeTruthy();
    // The cases to look at, the stopping ones first; the passing ones collapsed.
    const cases = within(box).getByRole('table', { name: 'Cases to look at' });
    const ids = within(cases)
      .getAllByRole('rowheader')
      .map((th) => th.textContent);
    expect(ids).toEqual(['safety/s-001', 'spindle/a-001', 'bearing/b-001', 'repro/r-001', 'new/n-001']);
    const safety = within(cases).getByRole('row', { name: /safety\/s-001/ });
    expect(within(safety).getByText('Safety')).toBeTruthy();
    expect(within(safety).getByText('5 of 5 runs passed')).toBeTruthy();
    expect(within(safety).getByText('4 of 5 runs passed')).toBeTruthy();
    expect(within(safety).getByText(/Stops the merge/)).toBeTruthy();
    expect(within(within(cases).getByRole('row', { name: /new\/n-001/ })).getByText('New case')).toBeTruthy();
    expect(within(within(cases).getByRole('row', { name: /repro\/r-001/ })).getByText(/Fixed/)).toBeTruthy();
    const passing = within(box).getByText('Show 1 passing case');
    expect((passing.closest('details') as HTMLDetailsElement).open).toBe(false);
    fireEvent.click(passing);
    expect(within(box).getByRole('table', { name: 'Passing cases' })).toBeTruthy();
    // The marks are for the eye only: the words carry the result.
    for (const mark of box.querySelectorAll('[aria-hidden="true"]')) expect(mark.textContent).toMatch(/^[✓✕•]$/);
    // The check list points to the table instead of repeating the Markdown.
    const checks = await section('Check results');
    const link = within(checks).getByRole('link', { name: 'The comparison with the baseline is under “AI evaluation” below' });
    expect(link.getAttribute('href')).toBe('#mr-ai-eval-title');
    expect(within(checks).queryByText(/\| Metric \|/)).toBeNull();
  });

  it('a passed evaluation says it does not stop the merge', async () => {
    await open([aiEval({ conclusion: 'success', summary: PASSED_SUMMARY })]);
    const box = await section(/AI evaluation/);
    expect(within(box).getByTestId('ai-eval-line').textContent).toContain(
      'The AI evaluation is not worse than the baseline. This item does not stop the merge.',
    );
    expect(within(box).queryByRole('heading', { level: 3 })).toBeNull();
    expect(within(box).queryByRole('table', { name: 'Cases to look at' })).toBeNull();
  });

  it('an error says why it stops and what to do next, with the pipeline, the reason code and the runbook', async () => {
    await open([aiEval({ conclusion: 'error', summary: ERROR_SUMMARY })]);
    const box = await section(/AI evaluation/);
    const line = within(box).getByTestId('ai-eval-line').textContent;
    expect(line).toContain('could not be compared with the baseline, so the merge is stopped (FG-AIASSET-0002). Next:');
    expect(line).toContain('left eval-report.json, then run the pipeline again.');
    expect(within(box).getByRole('link', { name: 'Open pipeline #12' }).getAttribute('href')).toMatch(/pipelines\/12$/);
    expect(within(box).getByText('bench')).toBeTruthy();
    expect(within(box).getByText('report_missing')).toBeTruthy();
    expect(within(box).getByText('ai-eval-bench')).toBeTruthy();
  });

  it('while it runs, says so; a summary that does not read is shown as written', async () => {
    const running = await open([aiEval({ status: 'in_progress', conclusion: null, summary: '' })]);
    expect(await within(await section(/AI evaluation/)).findByText(/The AI evaluation is running/)).toBeTruthy();
    running.unmount();
    await open([aiEval({ conclusion: 'failure', summary: 'something the screen does not know' })]);
    const box = await section(/AI evaluation/);
    expect(within(box).getByText(/could not be read as a table/)).toBeTruthy();
    expect(within(box).getByText('something the screen does not know')).toBeTruthy();
  });

  it('a reason the screen does not know reads as written, with its code', async () => {
    await open([
      aiEval({
        conclusion: 'failure',
        summary: GOLDEN.replace(
          '- [FG-AIASSET-0004] safety_case_failed `safety/s-001`: 4/5 passed',
          '- [FG-AIASSET-0004] brand_new_reason `safety/s-001`: something new',
        ),
      }),
    ]);
    const box = await section(/AI evaluation/);
    expect(within(box).getByText(/\[FG-AIASSET-0004\] brand_new_reason `safety\/s-001`: something new/)).toBeTruthy();
  });

  it('a long summary that does not read is cut, and says so', async () => {
    await open([aiEval({ conclusion: 'failure', summary: 'y'.repeat(UNREADABLE_SHOWN_MAX + 10) })]);
    const box = await section(/AI evaluation/);
    expect(within(box).getByText('y'.repeat(UNREADABLE_SHOWN_MAX))).toBeTruthy();
    expect(within(box).getByText(/only the beginning is shown/)).toBeTruthy();
  });

  it('a head without the check has no AI evaluation section', async () => {
    await open([checkRun(1, 'ci/build')]);
    await within(await section('Check results')).findByText('ci/build');
    expect(screen.queryByRole('heading', { level: 2, name: /AI evaluation/ })).toBeNull();
  });

  it('reads in Japanese', async () => {
    await open([aiEval({ conclusion: 'failure', summary: GOLDEN })], 'ja');
    const box = await section('AI の評価（承認済みの基準との比較）');
    expect(within(box).getByTestId('ai-eval-line').textContent).toContain('3 件あるため、マージを止めています。次に:');
    expect(within(box).getByRole('row', { name: /^原因の的中率/ })).toBeTruthy();
    expect(within(box).getAllByText('5 回中 5 回合格').length).toBeGreaterThan(0);
  });

  it('has no axe violations in ja and en', async () => {
    for (const lang of ['ja', 'en'] as const) {
      const view = await open([aiEval({ conclusion: 'failure', summary: GOLDEN })], lang);
      await section(lang === 'ja' ? /AI の評価/ : /AI evaluation/);
      const result = await axe.run(document.querySelector('main') as HTMLElement, {
        runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
        rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
      });
      expect(result.violations.map((v) => v.id)).toEqual([]);
      view.unmount();
    }
  });
});
