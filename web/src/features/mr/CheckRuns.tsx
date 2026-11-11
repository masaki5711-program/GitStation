// The detail under the one-line status (plan 08 §1.6): the check runs of the
// head on screen (name, namespace, result, summary, link, annotations with
// file:line) and the conflicts predicted with other open merge requests.
// The status line above stays the primary message; this is for whoever
// wants to know more. A long list is collapsed behind its counts, and the
// annotations of a run always are. Each part reads on its own: a failure
// shows in its place only, with its request_id.
import type { TFunction } from 'i18next';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import {
  isRunning,
  namespaceOf,
  orderRuns,
  safeDetailsUrl,
  useCheckRuns,
  useConflicts,
  type CheckAnnotation,
  type CheckRun,
  type Conflicts,
} from '../../api/checks';
import { pipelineIdOfCheck } from '../../api/pipelines';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import { checkBadge } from '../labels';
import { blobUrl, pipelineUrl, pullUrl } from '../repoUrls';
import { StateBadge } from './StateBadge';
import styles from './Mr.module.css';

/** More runs than this: the list is collapsed behind its counts. */
export const RUNS_OPEN_MAX = 5;

export function CheckRunsSection({
  projectId,
  project,
  iid,
  head,
}: {
  projectId: number;
  project: string;
  iid: number;
  /** The head on screen; null before the merge request has a version. */
  head: string | null;
}) {
  const { t } = useTranslation();
  const runs = useCheckRuns(projectId, iid, head);
  return (
    <section aria-labelledby="mr-check-runs-title" className={styles.detailSection}>
      <h2 id="mr-check-runs-title" className={styles.detailTitle}>
        {t('mrChecks.title')}
      </h2>
      {head === null && <p className={styles.nextHint}>{t('mrChecks.noHead')}</p>}
      {head !== null && runs.isPending && <Loading />}
      {runs.isError && (
        <QueryError
          error={runs.error}
          onRetry={() => {
            void runs.refetch();
          }}
        />
      )}
      {runs.isSuccess && head !== null && (
        <RunList runs={runs.data.check_runs} project={project} head={head} />
      )}
    </section>
  );
}

function RunList({ runs, project, head }: { runs: CheckRun[]; project: string; head: string }) {
  const { t } = useTranslation();
  if (runs.length === 0) return <p className={styles.nextHint}>{t('mrChecks.none')}</p>;
  const ordered = orderRuns(runs);
  const running = runs.filter(isRunning).length;
  const passed = runs.filter((r) => !isRunning(r) && (r.conclusion === 'success' || r.conclusion === 'neutral')).length;
  const failed = runs.length - running - passed;
  const counts = t('mrChecks.counts', { total: runs.length, failed, running, passed });
  const list = (
    <ul className={styles.runList} aria-label={t('mrChecks.listLabel')}>
      {ordered.map((r) => (
        <Run key={r.id} run={r} project={project} head={head} />
      ))}
    </ul>
  );
  if (runs.length <= RUNS_OPEN_MAX) {
    return (
      <>
        <p className={styles.nextHint}>{counts}</p>
        {list}
      </>
    );
  }
  return (
    <details className={styles.detailGroup}>
      <summary className={styles.resolvedSummary}>{counts}</summary>
      {list}
    </details>
  );
}

function Run({ run, project, head }: { run: CheckRun; project: string; head: string }) {
  const { t } = useTranslation();
  const badge = checkBadge(t, run.status, run.conclusion);
  const namespace = namespaceOf(run.name);
  const link = safeDetailsUrl(run.details_url);
  const pipelineId = pipelineIdOfCheck(run);
  return (
    <li className={styles.run}>
      <p className={styles.runHead}>
        <code>{run.name}</code> <StateBadge tone={badge.tone}>{badge.label}</StateBadge>
        {namespace !== null && (
          <span className={styles.nextHint}>{t('mrChecks.namespace', { namespace })}</span>
        )}
      </p>
      {run.summary.trim() !== '' && <p className={styles.runSummary}>{run.summary}</p>}
      {pipelineId !== null && (
        <p className={styles.runSummary}>
          <Link to={pipelineUrl(project, pipelineId)}>{t('mrChecks.openPipeline', { id: pipelineId })}</Link>
        </p>
      )}
      {link !== null && (
        <p className={styles.runSummary}>
          <a href={link} rel="noopener noreferrer">
            {t('mrChecks.details', { name: run.name })}{' '}
            {/* The cue: words for a screen reader, an arrow (CSS) for the eye. */}
            <span className={styles.srOnly}>{t('mrChecks.externalSite')}</span>
            <span aria-hidden="true" className={styles.externalMark} />
          </a>
        </p>
      )}
      {run.annotations.length > 0 && (
        <details className={styles.detailGroup}>
          <summary className={styles.resolvedSummary}>
            {t('mrChecks.annotations', { count: run.annotations.length })}
          </summary>
          <ul className={styles.plainList}>
            {run.annotations.map((a, i) => (
              <Annotation key={`${String(i)}-${a.path}-${String(a.start_line)}`} a={a} project={project} head={head} />
            ))}
          </ul>
        </details>
      )}
    </li>
  );
}

function levelLabel(t: TFunction, level: string): string {
  switch (level) {
    case 'failure':
      return t('mrChecks.levelFailure');
    case 'warning':
      return t('mrChecks.levelWarning');
    default:
      return t('mrChecks.levelNotice');
  }
}

/** `src/lib.rs:3` or `src/lib.rs:3-4`; a line 0 is the whole file. */
export function locationOf(a: Pick<CheckAnnotation, 'path' | 'start_line' | 'end_line'>): string {
  if (a.start_line <= 0) return a.path;
  if (a.end_line > a.start_line) return `${a.path}:${String(a.start_line)}-${String(a.end_line)}`;
  return `${a.path}:${String(a.start_line)}`;
}

function Annotation({ a, project, head }: { a: CheckAnnotation; project: string; head: string }) {
  const { t } = useTranslation();
  const to = `${blobUrl(project, head, a.path)}${a.start_line > 0 ? `#L${String(a.start_line)}` : ''}`;
  return (
    <li className={styles.annotation}>
      <Link to={to}>
        <code>{locationOf(a)}</code>
      </Link>{' '}
      <span className={a.level === 'failure' ? styles.levelFailure : styles.levelOther}>
        {levelLabel(t, a.level)}
      </span>{' '}
      {a.title !== null && a.title !== '' && <strong>{a.title}: </strong>}
      {a.message}
    </li>
  );
}

/** Says something only when there is something to say: a predicted
 * conflict, or a prediction that could not be finished. */
export function ConflictsSection({
  projectId,
  project,
  iid,
}: {
  projectId: number;
  project: string;
  iid: number;
}) {
  const { t } = useTranslation();
  const conflicts = useConflicts(projectId, iid);
  if (conflicts.isPending) return null;
  if (conflicts.isError) {
    return (
      <section aria-labelledby="mr-conflicts-title" className={styles.detailSection}>
        <h2 id="mr-conflicts-title" className={styles.detailTitle}>
          {t('mrChecks.conflictsTitle')}
        </h2>
        <QueryError
          error={conflicts.error}
          onRetry={() => {
            void conflicts.refetch();
          }}
        />
      </section>
    );
  }
  if (!worthShowing(conflicts.data)) return null;
  return (
    <section aria-labelledby="mr-conflicts-title" className={styles.detailSection}>
      <h2 id="mr-conflicts-title" className={styles.detailTitle}>
        {t('mrChecks.conflictsTitle')}
      </h2>
      <ConflictList data={conflicts.data} project={project} />
    </section>
  );
}

export function worthShowing(data: Conflicts): boolean {
  return (
    data.predictions.length > 0 || data.scan.status === 'failed' || data.scan.status === 'incomplete'
  );
}

function ConflictList({ data, project }: { data: Conflicts; project: string }) {
  const { t } = useTranslation();
  const conflicting = data.predictions.filter((p) => p.outcome === 'conflict');
  const undecided = data.predictions.filter((p) => p.outcome !== 'conflict');
  return (
    <>
      {conflicting.length > 0 && (
        <p className={styles.warning}>{t('mrChecks.conflictsSummary', { count: conflicting.length })}</p>
      )}
      {data.scan.status === 'failed' && <p className={styles.nextHint}>{t('mrChecks.scanFailed')}</p>}
      {data.scan.status === 'incomplete' && <p className={styles.nextHint}>{t('mrChecks.scanIncomplete')}</p>}
      {data.scan.stale && <p className={styles.nextHint}>{t('mrChecks.scanStale')}</p>}
      {data.predictions.length > 0 && (
        <ul className={styles.runList} aria-label={t('mrChecks.conflictsTitle')}>
          {conflicting.map((p) => (
            <li key={p.iid} className={styles.run}>
              <p className={styles.runHead}>
                <Link to={pullUrl(project, p.iid)}>{t('mr.number', { iid: p.iid })}</Link>
                {p.draft && <StateBadge tone="draft">{t('mr.draft')}</StateBadge>}
              </p>
              {p.conflicts.length > 0 && (
                <details className={styles.detailGroup}>
                  <summary className={styles.resolvedSummary}>
                    {t('mrChecks.conflictPaths', { count: p.conflicts.length })}
                  </summary>
                  <ul className={styles.plainList}>
                    {p.conflicts.map((path) => (
                      <li key={path}>
                        <code>{path}</code>
                      </li>
                    ))}
                  </ul>
                  {p.conflicts_truncated && <p className={styles.nextHint}>{t('mrChecks.conflictsTruncated')}</p>}
                </details>
              )}
            </li>
          ))}
          {undecided.map((p) => (
            <li key={p.iid} className={styles.run}>
              <Link to={pullUrl(project, p.iid)}>{t('mr.number', { iid: p.iid })}</Link>{' '}
              <span className={styles.nextHint}>{t('mrChecks.undecided')}</span>
            </li>
          ))}
        </ul>
      )}
    </>
  );
}
