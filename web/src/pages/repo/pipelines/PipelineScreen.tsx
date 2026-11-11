// One CI pipeline (`/-/pipelines/{id}[?job={job}]`): one line saying why it
// stopped and what to press next (plan 08), Cancel / Retry when the status
// and the caller's role allow them (the server decides again; a refusal
// shows its user_message and request_id), the diagnosis of a failed one,
// the problems of an invalid definition, the jobs by stage, and the chosen
// job's log. Read again every few seconds while it runs.
import { useQuery } from '@tanstack/react-query';
import type { TFunction } from 'i18next';
import { useTranslation } from 'react-i18next';
import { Link, useSearchParams } from 'react-router';

import { call } from '../../../api/client';
import { useApi } from '../../../api/context';
import {
  branchOf,
  canCancel,
  canRetry,
  mayOperate,
  parseId,
  useCancelPipeline,
  useDiagnosis,
  usePipeline,
  useRetryPipeline,
  type CiFailure,
  type Pipeline,
  type PipelineJob,
} from '../../../api/pipelines';
import { Loading } from '../../../components/Loading';
import { QueryError } from '../../../components/QueryError';
import controls from '../../../components/controls.module.css';
import { formatDateTime, formatRelative } from '../../../features/labels';
import { StateBadge } from '../../../features/mr/StateBadge';
import {
  formatDuration,
  pipelineSourceLabel,
  pipelineStatusLabel,
  pipelineTone,
  triggeredByLabel,
} from '../../../features/pipelineLabels';
import { blobUrl, commitUrl, pipelineUrl, pipelinesUrl, pullUrl } from '../../../features/repoUrls';
import { useDocumentTitle } from '../../../hooks/useDocumentTitle';
import { useSession } from '../../../session/session';
import { RepoError } from '../common';
import repo from '../Repo.module.css';
import { JobLog } from './JobLog';
import styles from './Pipelines.module.css';

/** The CI definition's file (ADR-0029). */
const CI_FILE = '.forge-ci.yml';

export function PipelineScreen({
  projectId,
  project,
  id,
}: {
  projectId: number;
  project: string;
  id: number;
}) {
  const { t } = useTranslation();
  const pipeline = usePipeline(projectId, id);
  const title = t('pipelines.number', { id });
  useDocumentTitle(title);
  if (pipeline.isPending) return <Loading />;
  if (pipeline.isError) {
    return (
      <>
        <h1 className={repo.title}>{title}</h1>
        <RepoError
          error={pipeline.error}
          onRetry={() => {
            void pipeline.refetch();
          }}
        />
        <p>
          <Link to={pipelinesUrl(project)}>{t('pipelines.backToList')}</Link>
        </p>
      </>
    );
  }
  return <PipelineView projectId={projectId} project={project} p={pipeline.data} title={title} />;
}

function PipelineView({
  projectId,
  project,
  p,
  title,
}: {
  projectId: number;
  project: string;
  p: Pipeline;
  title: string;
}) {
  const { t, i18n } = useTranslation();
  const api = useApi();
  const session = useSession();
  const [params] = useSearchParams();
  // The same query (and key) as the project's frame: no second request.
  const info = useQuery({
    queryKey: ['project', projectId],
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/projects/{id}', { signal, params: { path: { id: projectId } } })),
  });
  const operate = info.isSuccess && mayOperate(info.data.role, session.user.is_admin);
  const failed = p.status === 'failed';
  const diagnosis = useDiagnosis(projectId, p.id, failed);
  const jobs = p.jobs ?? [];
  const jobId = parseId(params.get('job'));
  const openJob = jobId === null ? undefined : jobs.find((j) => j.id === jobId);
  const duration = formatDuration(t, p.created_at, p.finished_at);
  const status = pipelineStatusLabel(t, p.status);

  return (
    <>
      <p className={styles.hint}>
        <Link to={pipelinesUrl(project)}>{t('pipelines.backToList')}</Link>
      </p>
      <h1 className={repo.title}>
        {title} <StateBadge tone={pipelineTone(p.status)}>{status}</StateBadge>
      </h1>
      {/* The status alone, announced when it changes (not the whole line). */}
      <p role="status" className={styles.srOnly}>
        {t('pipelines.statusNow', { status })}
      </p>
      <p className={styles.meta}>
        <code className={styles.ref}>{branchOf(p.ref)}</code>
        <Link to={commitUrl(project, p.sha)} aria-label={t('pipelines.openCommit', { sha: p.sha.slice(0, 8) })}>
          <code>{p.sha.slice(0, 8)}</code>
        </Link>
        <span>{pipelineSourceLabel(t, p.source)}</span>
        <span>{triggeredByLabel(t, p.triggered_by, session.user.id)}</span>
        {duration !== null && <span className={styles.numeric}>{t('pipelines.took', { duration })}</span>}
        <time dateTime={p.created_at} title={formatDateTime(p.created_at, i18n.language)}>
          {formatRelative(p.created_at, i18n.language)}
        </time>
      </p>

      <StatusLine p={p} jobs={jobs} operate={operate} project={project} />
      {operate && <Actions projectId={projectId} p={p} />}

      {p.status === 'invalid' && p.problems.length > 0 && (
        <section aria-labelledby="pipeline-problems">
          <h2 id="pipeline-problems" className={styles.sectionTitle}>
            {t('pipelines.problems.title')}
          </h2>
          <ul className={styles.problems}>
            {p.problems.map((pr, i) => (
              <li key={`${String(i)}-${String(pr.line)}`}>
                <Link to={`${blobUrl(project, p.sha, CI_FILE)}${pr.line > 0 ? `#L${String(pr.line)}` : ''}`}>
                  {pr.line > 0
                    ? t('pipelines.problems.line', { line: pr.line })
                    : t('pipelines.problems.file')}
                </Link>{' '}
                <code>{pr.reason}</code>
                {pr.subject !== null && pr.subject !== '' && <> {'·'} <code>{pr.subject}</code></>}
              </li>
            ))}
          </ul>
        </section>
      )}

      {failed && (
        <section aria-labelledby="pipeline-diagnosis" data-testid="pipeline-diagnosis">
          <h2 id="pipeline-diagnosis" className={styles.sectionTitle}>
            {t('pipelines.diagnosis.title')}
          </h2>
          {diagnosis.isPending && <Loading />}
          {diagnosis.isError && (
            <QueryError
              error={diagnosis.error}
              onRetry={() => {
                void diagnosis.refetch();
              }}
            />
          )}
          {diagnosis.isSuccess &&
            (diagnosis.data.failures.length === 0 ? (
              <p className={styles.hint}>{t('pipelines.diagnosis.none')}</p>
            ) : (
              diagnosis.data.failures.map((f) => (
                <FailureView key={`${String(f.job_id)}-${String(f.attempt)}`} f={f} project={project} pipelineId={p.id} />
              ))
            ))}
        </section>
      )}

      <section aria-labelledby="pipeline-jobs">
        <h2 id="pipeline-jobs" className={styles.sectionTitle}>
          {t('pipelines.jobs.title')}
        </h2>
        {jobs.length === 0 ? (
          <p className={styles.hint}>{t('pipelines.jobs.none')}</p>
        ) : (
          <Stages jobs={jobs} project={project} pipelineId={p.id} openJob={openJob?.id ?? null} />
        )}
      </section>

      {jobId !== null && openJob === undefined && (
        <p className={styles.hint}>{t('pipelines.log.noSuchJob')}</p>
      )}
      {openJob !== undefined && <JobLog key={openJob.id} projectId={projectId} pipelineId={p.id} job={openJob} />}
    </>
  );
}

/** One line: why it is where it is, and what to press next. */
function StatusLine({
  p,
  jobs,
  operate,
  project,
}: {
  p: Pipeline;
  jobs: PipelineJob[];
  operate: boolean;
  project: string;
}) {
  const { t } = useTranslation();
  const failedJob = jobs.find((j) => j.status === 'failed');
  const tone =
    p.status === 'failed' || p.status === 'invalid' || p.status === 'error'
      ? styles.statusFailed
      : p.status === 'succeeded'
        ? styles.statusDone
        : '';
  return (
    <div className={`${styles.statusLine} ${tone ?? ''}`} data-testid="pipeline-status-line">
      <p>{statusSentence(t, p.status, failedJob?.name ?? null, operate)}</p>
      {failedJob !== undefined && (
        <p>
          <Link to={pipelineUrl(project, p.id, failedJob.id)}>
            {t('pipelines.openFailedLog', { job: failedJob.name })}
          </Link>
        </p>
      )}
    </div>
  );
}

function statusSentence(t: TFunction, status: string, job: string | null, operate: boolean): string {
  switch (status) {
    case 'pending':
      return t('pipelines.line.pending');
    case 'running':
      return t('pipelines.line.running');
    case 'succeeded':
      return t('pipelines.line.succeeded');
    case 'skipped':
      return t('pipelines.line.skipped');
    case 'failed':
      if (job === null) return operate ? t('pipelines.line.failedRetry') : t('pipelines.line.failed');
      return operate
        ? t('pipelines.line.failedJobRetry', { job })
        : t('pipelines.line.failedJob', { job });
    case 'canceled':
      return operate ? t('pipelines.line.canceledRetry') : t('pipelines.line.canceled');
    case 'invalid':
      return t('pipelines.line.invalid');
    case 'error':
      return t('pipelines.line.error');
    default:
      return pipelineStatusLabel(t, status);
  }
}

/** Cancel (pending / running) and Retry (failed / canceled). */
function Actions({ projectId, p }: { projectId: number; p: Pipeline }) {
  const { t } = useTranslation();
  const cancel = useCancelPipeline(projectId, p.id);
  const retry = useRetryPipeline(projectId, p.id);
  const busy = cancel.isPending || retry.isPending;
  const showCancel = canCancel(p.status);
  const showRetry = canRetry(p.status);
  if (!showCancel && !showRetry && !cancel.isError && !retry.isError) return null;
  return (
    <div data-testid="pipeline-actions">
      <p className={styles.actions}>
        {showCancel && (
          <button
            type="button"
            className={controls.button}
            disabled={busy}
            onClick={() => {
              retry.reset();
              cancel.mutate();
            }}
          >
            {t('pipelines.cancel')}
          </button>
        )}
        {showRetry && (
          <button
            type="button"
            className={controls.primary}
            disabled={busy}
            onClick={() => {
              cancel.reset();
              retry.mutate();
            }}
          >
            {t('pipelines.retry')}
          </button>
        )}
      </p>
      {cancel.isError && <QueryError error={cancel.error} />}
      {retry.isError && <QueryError error={retry.error} />}
    </div>
  );
}

function FailureView({ f, project, pipelineId }: { f: CiFailure; project: string; pipelineId: number }) {
  const { t, i18n } = useTranslation();
  return (
    <article className={styles.failure} data-testid="pipeline-failure">
      <h3>{t('pipelines.diagnosis.job', { job: f.job, stage: f.stage })}</h3>
      {f.pending ? (
        <p className={styles.hint}>{t('pipelines.diagnosis.pending')}</p>
      ) : (
        <>
          {f.excerpt !== '' && <pre className={styles.log}>{f.excerpt}</pre>}
          {f.occurrences > 1 && (
            <p className={styles.hint}>
              {f.first_seen_at === null
                ? t('pipelines.diagnosis.seen', { count: f.occurrences })
                : t('pipelines.diagnosis.seenSince', {
                    count: f.occurrences,
                    date: formatRelative(f.first_seen_at, i18n.language),
                  })}
            </p>
          )}
          {f.known_fixes.length > 0 && (
            <>
              <p>{t('pipelines.diagnosis.knownFixes')}</p>
              <ul className={styles.problems}>
                {f.known_fixes.map((k) => (
                  <li key={k.merge_request_id}>
                    <Link to={pullUrl(project, k.iid)}>
                      {t('pipelines.diagnosis.fix', { iid: k.iid, title: k.title })}
                    </Link>
                  </li>
                ))}
              </ul>
            </>
          )}
        </>
      )}
      <p>
        <Link to={pipelineUrl(project, pipelineId, f.job_id)}>
          {t('pipelines.diagnosis.openLog', { job: f.job })}
        </Link>
      </p>
    </article>
  );
}

/** The jobs as a list per stage, stages in the order their first job runs
 * (wave, then id). */
export function groupByStage(jobs: readonly PipelineJob[]): { stage: string; jobs: PipelineJob[] }[] {
  const sorted = [...jobs].sort((a, b) => a.wave - b.wave || a.id - b.id);
  const groups: { stage: string; jobs: PipelineJob[] }[] = [];
  for (const j of sorted) {
    const g = groups.find((x) => x.stage === j.stage);
    if (g === undefined) groups.push({ stage: j.stage, jobs: [j] });
    else g.jobs.push(j);
  }
  return groups;
}

function Stages({
  jobs,
  project,
  pipelineId,
  openJob,
}: {
  jobs: PipelineJob[];
  project: string;
  pipelineId: number;
  openJob: number | null;
}) {
  const { t } = useTranslation();
  return (
    <>
      {groupByStage(jobs).map((g) => (
        <div key={g.stage}>
          <h3 className={styles.stageTitle}>{t('pipelines.jobs.stage', { stage: g.stage })}</h3>
          <ul className={styles.jobs}>
            {g.jobs.map((j) => {
              const duration = formatDuration(t, j.started_at, j.finished_at);
              return (
                <li key={j.id} className={styles.job} data-testid="pipeline-job">
                  <span className={styles.itemMain}>
                    <StateBadge tone={pipelineTone(j.status)}>{pipelineStatusLabel(t, j.status)}</StateBadge>
                    <code>{j.name}</code>
                  </span>
                  <span className={styles.meta}>
                    {j.attempt > 1 && <span>{t('pipelines.jobs.attempt', { attempt: j.attempt })}</span>}
                    {j.needs.length > 0 && (
                      <span>{t('pipelines.jobs.needs', { needs: j.needs.join(', ') })}</span>
                    )}
                    {duration !== null && (
                      <span className={styles.numeric}>{t('pipelines.took', { duration })}</span>
                    )}
                    <Link
                      to={pipelineUrl(project, pipelineId, j.id)}
                      aria-label={t('pipelines.jobs.logOf', { job: j.name })}
                      {...(openJob === j.id ? { 'aria-current': 'true' as const } : {})}
                    >
                      {t('pipelines.jobs.log')}
                    </Link>
                  </span>
                </li>
              );
            })}
          </ul>
        </div>
      ))}
    </>
  );
}
