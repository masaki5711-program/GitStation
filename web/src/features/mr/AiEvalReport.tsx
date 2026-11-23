// The AI evaluation of the head on screen (plan 17 P17-9): the comparison of
// the bench's eval-report.json with the approved baseline that the
// `forge/ai-eval` check wrote (ADR-0034), in words an engineer on the
// manufacturing side reads without knowing the bench. One line first: why
// it stops the merge and what to do next (plan 08); then what stops it,
// the measures and the cases. Every result is a word (with a mark for the
// eye), never a colour alone. Shown only when the head has the check.
import type { TFunction } from 'i18next';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { isRunning, useCheckRuns, type CheckRun } from '../../api/checks';
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
} from './aiEval';
import styles from './Mr.module.css';

export const AI_EVAL_TITLE_ID = 'mr-ai-eval-title';

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
  const run = runs.isSuccess ? aiEvalRun(runs.data.check_runs) : undefined;
  if (run === undefined) return null;
  return (
    <section aria-labelledby={AI_EVAL_TITLE_ID} className={styles.detailSection}>
      <h2 id={AI_EVAL_TITLE_ID} className={styles.detailTitle}>
        {t('aiEval.title')}
      </h2>
      <AiEvalBody run={run} project={project} />
    </section>
  );
}

function AiEvalBody({ run, project }: { run: CheckRun; project: string }) {
  const { t } = useTranslation();
  if (isRunning(run)) return <p className={styles.nextHint}>{t('aiEval.running')}</p>;
  const report = parseAiEvalSummary(run.summary);
  if (report === null) {
    // Never an empty table: the summary as written, and why.
    return (
      <>
        <p className={styles.nextHint}>{t('aiEval.unreadable')}</p>
        <p className={styles.runSummary}>{run.summary}</p>
      </>
    );
  }
  if (report.kind === 'error') return <ErrorReport report={report} project={project} />;
  return <Evaluated report={report} />;
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

function Evaluated({ report }: { report: AiEvalEvaluated }) {
  const { t } = useTranslation();
  const blocking = report.findings.length;
  const open = report.cases.filter((c) => c.result !== 'pass');
  const passing = report.cases.filter((c) => c.result === 'pass');
  return (
    <>
      {report.passed ? (
        <p className={styles.statusOk} data-testid="ai-eval-line">
          <Mark kind="ok" /> {t('aiEval.passedLine')}
        </p>
      ) : (
        <p className={styles.warning} data-testid="ai-eval-line">
          <Mark kind="stop" /> {t('aiEval.failedLine', { count: blocking })}
        </p>
      )}
      <p className={styles.nextHint}>
        {t('aiEval.overview', {
          models: report.models,
          total: report.total,
          safety: report.safety,
          reproduction: report.reproduction,
        })}{' '}
        {report.flaky > 0 && t('aiEval.flakyNote', { count: report.flaky })}
      </p>
      {report.findings.length > 0 && (
        <>
          <h3 className={styles.subTitle}>{t('aiEval.findingsTitle')}</h3>
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
function Mark({ kind }: { kind: 'ok' | 'stop' | 'note' }) {
  const glyph = kind === 'ok' ? '✓' : kind === 'stop' ? '✕' : '•';
  return (
    <span aria-hidden="true" className={styles.mark}>
      {glyph}
    </span>
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
  );
}

function runCount(t: TFunction, c: RunCount | null): string {
  return c === null ? t('aiEval.newCase') : t('aiEval.runCount', { passes: c.passes, runs: c.runs });
}

function CaseTable({ cases, caption }: { cases: AiEvalCase[]; caption: string }) {
  const { t } = useTranslation();
  return (
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
              <code>{c.id}</code>
            </th>
            <td>{kindLabel(t, c.kind)}</td>
            <td>{runCount(t, c.baseline)}</td>
            <td>{runCount(t, c.now)}</td>
            <td>
              <CaseResultLabel result={c.result} />
            </td>
          </tr>
        ))}
      </tbody>
    </table>
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
