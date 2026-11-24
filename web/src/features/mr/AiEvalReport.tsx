// The AI evaluation of the head on screen (plan 17 P17-9): the comparison of
// the bench's eval-report.json with the approved baseline that the
// `forge/ai-eval` check wrote (ADR-0034), in words an engineer on the
// manufacturing side reads without knowing the bench. One line first: why
// it stops the merge and what to do next (plan 08); then what stops it,
// the measures and the cases. Every result is a word (with a mark for the
// eye), never a colour alone. Shown only when the head has the check.
import type { TFunction } from 'i18next';
import type { ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { isRunning, useCheckRuns, type CheckRun } from '../../api/checks';
import { pipelineIdOfCheck } from '../../api/pipelines';
import { QueryError } from '../../components/QueryError';
import { pipelineUrl } from '../repoUrls';
import {
  AI_EVAL_CHECK,
  isRate,
  parseAiEvalSummary,
  worseWhenLower,
  type AiEvalCase,
  type AiEvalCaseResult,
  type AiEvalError,
  type AiEvalEvaluated,
  type AiEvalFinding,
  type AiEvalMetric,
  type RunCount,
  AI_EVAL_TITLE_ID,
} from './aiEval';
import styles from './Mr.module.css';

const FINDINGS_ID = 'mr-ai-eval-findings';

/** A summary that does not read is shown as written, up to this many characters. */
export const UNREADABLE_SHOWN_MAX = 4096;

/** Marks for the eye only (aria-hidden); the words beside them carry the meaning. */
const GLYPHS = { ok: '\u2713', stop: '\u2715', note: '\u2022' } as const;

/** The `forge/ai-eval` run of the head, if the head has one. */
export function aiEvalRun(runs: readonly CheckRun[]): CheckRun | undefined {
  return runs.find((r) => r.name === AI_EVAL_CHECK);
}

export function AiEvalSection({
  projectId,
  project,
  iid,
  head,
}: {
  projectId: number;
  project: string;
  iid: number;
  head: string | null;
}) {
  const { t } = useTranslation();
  // The same query as the check list above: one read for both.
  const runs = useCheckRuns(projectId, iid, head);
  // Whether this section exists at all depends on that read: before its
  // first answer (loading, or refused) nothing here is known, and the
  // "Check results" section above already shows the loading line and the
  // refusal with its request_id for the very same query, so it is not
  // shown twice. A refused re-read while the last answer is still on
  // screen is said here, in one line, under the (older) comparison.
  const all = runs.data?.check_runs;
  const run = all === undefined ? undefined : aiEvalRun(all);
  if (all === undefined || run === undefined) return null;
  const pipeline = all.map(pipelineIdOfCheck).find((id) => id !== null) ?? null;
  return (
    <section aria-labelledby={AI_EVAL_TITLE_ID} className={styles.detailSection}>
      <h2 id={AI_EVAL_TITLE_ID} className={styles.detailTitle}>
        {t('aiEval.title')}
      </h2>
      {runs.isError && (
        <QueryError
          error={runs.error}
          onRetry={() => {
            void runs.refetch();
          }}
        />
      )}
      <AiEvalBody run={run} project={project} pipeline={pipeline} />
    </section>
  );
}

function AiEvalBody({ run, project, pipeline }: { run: CheckRun; project: string; pipeline: number | null }) {
  const { t } = useTranslation();
  if (isRunning(run)) return <p className={styles.nextHint}>{t('aiEval.running')}</p>;
  const report = parseAiEvalSummary(run.summary);
  if (report === null) {
    // Never an empty table: the summary as written (bounded), and why.
    const cut = run.summary.length > UNREADABLE_SHOWN_MAX;
    return (
      <>
        <p className={styles.nextHint}>{t('aiEval.unreadable')}</p>
        <p className={styles.runSummary}>{cut ? run.summary.slice(0, UNREADABLE_SHOWN_MAX) : run.summary}</p>
        {cut && <p className={styles.nextHint}>{t('aiEval.unreadableCut')}</p>}
      </>
    );
  }
  if (report.kind === 'error') return <ErrorReport report={report} project={project} />;
  return <Evaluated report={report} project={project} pipeline={pipeline} />;
}

function ErrorReport({ report, project }: { report: AiEvalError; project: string }) {
  const { t } = useTranslation();
  return (
    <>
      <p className={styles.warning} data-testid="ai-eval-line">
        <Mark kind="stop" /> {t('aiEval.errorLine', { code: report.code })} {errorNext(t, report.code)}
      </p>
      <ul className={styles.plainList}>
        {report.pipelineId !== null && (
          <li>
            <Link to={pipelineUrl(project, report.pipelineId)}>
              {t('mrChecks.openPipeline', { id: report.pipelineId })}
            </Link>
            {report.job !== null && (
              <>
                {' '}
                {t('aiEval.job')} <code>{report.job}</code>
              </>
            )}
          </li>
        )}
        <li>
          {t('aiEval.reasonCode')} <code>{report.reason}</code>
        </li>
        {report.runbook !== null && (
          <li>
            {t('aiEval.runbook')} <code>{report.runbook}</code>
          </li>
        )}
      </ul>
    </>
  );
}

/** What to do next, by code (the catalog's fix, in plain words). */
function errorNext(t: TFunction, code: string): string {
  switch (code) {
    case 'FG-AIASSET-0002':
      return t('aiEval.errorNext.noReport');
    case 'FG-AIASSET-0003':
      return t('aiEval.errorNext.noBaseline');
    case 'FG-AIASSET-0007':
      return t('aiEval.errorNext.malformed');
    default:
      return t('aiEval.errorNext.other');
  }
}

function Evaluated({
  report,
  project,
  pipeline,
}: {
  report: AiEvalEvaluated;
  project: string;
  pipeline: number | null;
}) {
  const { t } = useTranslation();
  const blocking = report.findings.length;
  const open = report.cases.filter((c) => c.result !== 'pass');
  const passing = report.cases.filter((c) => c.result === 'pass');
  return (
    <>
      {report.passed ? (
        <p className={styles.statusReady} data-testid="ai-eval-line">
          <Mark kind="ok" /> {t('aiEval.passedLine')}
        </p>
      ) : (
        <p className={styles.warning} data-testid="ai-eval-line">
          <Mark kind="stop" /> {t('aiEval.failedLine', { count: blocking })}{' '}
          {blocking > 0 && <a href={`#${FINDINGS_ID}`}>{t('aiEval.seeFindings')}</a>}
          {pipeline !== null && (
            <>
              {' '}
              <Link to={pipelineUrl(project, pipeline)}>{t('mrChecks.openPipeline', { id: pipeline })}</Link>
            </>
          )}
        </p>
      )}
      <p className={styles.nextHint}>
        {t('aiEval.overview', {
          models: report.models,
          total: report.total,
          safety: report.safety,
          reproduction: report.reproduction,
        })}
      </p>
      {report.flaky > 0 && <p className={styles.evalNote}>{t('aiEval.flakyNote', { count: report.flaky })}</p>}
      {report.findings.length > 0 && (
        <>
          <h3 id={FINDINGS_ID} className={styles.subTitle}>
            {t('aiEval.findingsTitle')}
          </h3>
          <ul className={styles.reasonList}>
            {report.findings.map((f, i) => (
              <li key={`${String(i)}-${f.reason}-${f.subject ?? ''}`}>
                <FindingLine f={f} />
              </li>
            ))}
          </ul>
          {report.findingsCut && <p className={styles.nextHint}>{t('aiEval.findingsCut')}</p>}
        </>
      )}
      <MetricTable metrics={report.metrics} />
      {open.length > 0 && <CaseTable cases={open} caption={t('aiEval.casesTitle')} />}
      {passing.length > 0 && (
        <details className={styles.detailGroup}>
          <summary className={styles.resolvedSummary}>{t('aiEval.passingCases', { count: passing.length })}</summary>
          <CaseTable cases={passing} caption={t('aiEval.passingCaption')} />
        </details>
      )}
      {report.more > 0 && <p className={styles.nextHint}>{t('aiEval.more', { count: report.more })}</p>}
    </>
  );
}

/** A finding in plain words; a reason this screen does not know reads as written. */
export function findingText(t: TFunction, f: AiEvalFinding): string {
  const subject = f.subject ?? '';
  switch (f.reason) {
    case 'safety_case_failed':
      return t('aiEval.reason.safetyFailed', { subject });
    case 'safety_case_missing':
      return t('aiEval.reason.safetyMissing', { subject });
    case 'safety_runs_short':
      return t('aiEval.reason.safetyRunsShort', { subject });
    case 'reproduction_not_fixed':
      return t('aiEval.reason.reproductionNotFixed', { subject });
    case 'case_not_reproducing':
      return t('aiEval.reason.caseNotReproducing', { subject });
    case 'stable_case_regressed':
      return t('aiEval.reason.stableRegressed', { subject });
    case 'case_missing':
      return t('aiEval.reason.caseMissing', { subject });
    case 'case_kind_changed':
      return t('aiEval.reason.caseKindChanged', { subject });
    case 'metric_below_baseline':
      return t('aiEval.reason.metricPastDelta', { subject: metricName(t, subject) });
    default:
      return f.text;
  }
}

function FindingLine({ f }: { f: AiEvalFinding }) {
  const { t } = useTranslation();
  return (
    <>
      {findingText(t, f)} <code>{f.code}</code>
    </>
  );
}

/** A mark for the eye; the words beside it carry the meaning. */
function Mark({ kind }: { kind: keyof typeof GLYPHS }) {
  return (
    <span aria-hidden="true" className={styles.mark}>
      {GLYPHS[kind]}
    </span>
  );
}

/** A table that scrolls sideways on a narrow screen instead of the page. */
function Scroll({ label, children }: { label: string; children: ReactNode }) {
  return (
    // Focusable so a keyboard can scroll it (WCAG 2.1.1); named by its table.
    <div className={styles.evalScroll} role="region" aria-label={label} tabIndex={0}>
      {children}
    </div>
  );
}

function metricName(t: TFunction, name: string): string {
  switch (name) {
    case 'cause_hit_rate':
      return t('aiEval.metric.causeHitRate');
    case 'false_alarm_rate':
      return t('aiEval.metric.falseAlarmRate');
    case 'p95_latency_ms':
      return t('aiEval.metric.p95Latency');
    case 'attach_precision':
      return t('aiEval.metric.attachPrecision');
    case 'abstain_accuracy':
      return t('aiEval.metric.abstainAccuracy');
    default:
      return name;
  }
}

function kindLabel(t: TFunction, kind: AiEvalCase['kind']): string {
  switch (kind) {
    case 'safety':
      return t('aiEval.kind.safety');
    case 'reproduction':
      return t('aiEval.kind.reproduction');
    case 'regular':
      return t('aiEval.kind.regular');
  }
}

function caseResultLabel(t: TFunction, result: AiEvalCaseResult): string {
  switch (result) {
    case 'blocking':
      return t('aiEval.caseResult.blocking');
    case 'flaky':
      return t('aiEval.caseResult.flaky');
    case 'fixed':
      return t('aiEval.caseResult.fixed');
    case 'fail':
      return t('aiEval.caseResult.fail');
    case 'pass':
      return t('aiEval.caseResult.pass');
  }
}

function MetricTable({ metrics }: { metrics: AiEvalMetric[] }) {
  const { t, i18n } = useTranslation();
  const pct = new Intl.NumberFormat(i18n.language, { style: 'percent', minimumFractionDigits: 1, maximumFractionDigits: 1 });
  const pt = new Intl.NumberFormat(i18n.language, { minimumFractionDigits: 1, maximumFractionDigits: 1, signDisplay: 'exceptZero' });
  const sec = new Intl.NumberFormat(i18n.language, { minimumFractionDigits: 1, maximumFractionDigits: 1 });
  const value = (m: AiEvalMetric, v: number) => (isRate(m.name) ? pct.format(v) : t('aiEval.seconds', { value: sec.format(v / 1000) }));
  const delta = (m: AiEvalMetric) =>
    isRate(m.name)
      ? t('aiEval.points', { value: pt.format((m.now - m.baseline) * 100) })
      : t('aiEval.seconds', { value: pt.format((m.now - m.baseline) / 1000) });
  const allowed = (m: AiEvalMetric) => {
    if (m.allowed === null) return t('aiEval.allowedNone');
    const sign = worseWhenLower(m.name) ? -1 : 1;
    const v = isRate(m.name)
      ? t('aiEval.points', { value: pt.format(sign * m.allowed * 100) })
      : t('aiEval.seconds', { value: pt.format((sign * m.allowed) / 1000) });
    return t('aiEval.allowedUpTo', { value: v });
  };
  return (
    <Scroll label={t('aiEval.metricsTitle')}>
    <table className={styles.evalTable}>
      <caption className={styles.evalCaption}>{t('aiEval.metricsTitle')}</caption>
      <thead>
        <tr>
          <th scope="col">{t('aiEval.col.metric')}</th>
          <th scope="col" className={styles.num}>
            {t('aiEval.col.baseline')}
          </th>
          <th scope="col" className={styles.num}>
            {t('aiEval.col.now')}
          </th>
          <th scope="col" className={styles.num}>
            {t('aiEval.col.delta')}
          </th>
          <th scope="col">{t('aiEval.col.allowed')}</th>
          <th scope="col">{t('aiEval.col.result')}</th>
        </tr>
      </thead>
      <tbody>
        {metrics.map((m) => (
          <tr key={m.name}>
            <th scope="row">{metricName(t, m.name)}</th>
            <td className={styles.num}>{value(m, m.baseline)}</td>
            <td className={styles.num}>{value(m, m.now)}</td>
            <td className={styles.num}>{delta(m)}</td>
            <td>{allowed(m)}</td>
            <td>
              {m.result === 'past_delta' ? (
                <strong className={styles.levelFailure}>
                  <Mark kind="stop" /> {t('aiEval.metricResult.pastDelta')}
                </strong>
              ) : m.result === 'ok' ? (
                <span>
                  <Mark kind="ok" /> {t('aiEval.metricResult.ok')}
                </span>
              ) : (
                <span className={styles.levelOther}>{t('aiEval.metricResult.shown')}</span>
              )}
            </td>
          </tr>
        ))}
      </tbody>
    </table>
    </Scroll>
  );
}

function runCount(t: TFunction, c: RunCount | null): string {
  return c === null ? t('aiEval.newCase') : t('aiEval.runCount', { passes: c.passes, runs: c.runs });
}

function CaseTable({ cases, caption }: { cases: AiEvalCase[]; caption: string }) {
  const { t } = useTranslation();
  return (
    <Scroll label={caption}>
    <table className={styles.evalTable}>
      <caption className={styles.evalCaption}>{caption}</caption>
      <thead>
        <tr>
          <th scope="col">{t('aiEval.col.case')}</th>
          <th scope="col">{t('aiEval.col.kind')}</th>
          <th scope="col">{t('aiEval.col.baseline')}</th>
          <th scope="col">{t('aiEval.col.now')}</th>
          <th scope="col">{t('aiEval.col.result')}</th>
        </tr>
      </thead>
      <tbody>
        {cases.map((c) => (
          <tr key={c.id}>
            <th scope="row">
              <code title={c.id}>{c.id}</code>
            </th>
            <td>{kindLabel(t, c.kind)}</td>
            <td className={styles.tabular}>{runCount(t, c.baseline)}</td>
            <td className={styles.tabular}>{runCount(t, c.now)}</td>
            <td>
              <CaseResultLabel result={c.result} />
            </td>
          </tr>
        ))}
      </tbody>
    </table>
    </Scroll>
  );
}

function CaseResultLabel({ result }: { result: AiEvalCaseResult }) {
  const { t } = useTranslation();
  const label = caseResultLabel(t, result);
  if (result === 'blocking')
    return (
      <strong className={styles.levelFailure}>
        <Mark kind="stop" /> {label}
      </strong>
    );
  if (result === 'pass' || result === 'fixed')
    return (
      <span>
        <Mark kind="ok" /> {label}
      </span>
    );
  return (
    <span className={styles.levelOther}>
      <Mark kind="note" /> {label}
    </span>
  );
}
