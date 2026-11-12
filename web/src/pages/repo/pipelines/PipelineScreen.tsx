// One CI pipeline (`/-/pipelines/{id}[?job={job}]`): one line saying why it
// stopped and what to press next (plan 08), Cancel / Retry when the status
// and the caller's role allow them (Actions.tsx), the diagnosis of a failed
// one (FailureView.tsx), the problems of an invalid definition, the jobs by
// stage (Stages.tsx), and the chosen job's log (JobLog.tsx). Read again
// every few seconds while it runs.
import { useQuery } from '@tanstack/react-query';
import type { TFunction } from 'i18next';
import { useTranslation } from 'react-i18next';
import { Link, useSearchParams } from 'react-router';

import { call } from '../../../api/client';
import { useApi } from '../../../api/context';
import {
  branchOf,
  mayOperate,
  parseId,
  useDiagnosis,
  usePipeline,
  type Pipeline,
  type PipelineJob,
} from '../../../api/pipelines';
import { Loading } from '../../../components/Loading';
import { QueryError } from '../../../components/QueryError';
import { formatDateTime, formatRelative } from '../../../features/labels';
import {
  formatDuration,
  formatSpan,
  pipelineSourceLabel,
  pipelineStatusLabel,
  triggeredByLabel,
} from '../../../features/pipelineLabels';
import { blobUrl, commitUrl, pipelineUrl, pipelinesUrl } from '../../../features/repoUrls';
import { useDocumentTitle } from '../../../hooks/useDocumentTitle';
import { useSession } from '../../../session/session';
import { RepoError } from '../common';
import { PipelineActions } from './Actions';
import { FailureView } from './FailureView';
import { JobLog } from './JobLog';
import { PipelineBadge } from './PipelineBadge';
import { Stages } from './Stages';
import styles from './Pipelines.module.css';
import { PageHeader } from '../../../ui/PageHeader';

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
        <PageHeader title={title} />
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
      <PageHeader
        title={
          <>
            {title} <PipelineBadge status={p.status} />
          </>
        }
      />
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
        {duration !== null && (
          <span
            className={styles.numeric}
            title={formatSpan(t, p.created_at, p.finished_at, i18n.language) ?? undefined}
          >
            {t('pipelines.took', { duration })}
          </span>
        )}
        <time dateTime={p.created_at} title={formatDateTime(p.created_at, i18n.language)}>
          {formatRelative(p.created_at, i18n.language)}
        </time>
      </p>

      <StatusLine p={p} jobs={jobs} operate={operate} project={project} />
      {operate && <PipelineActions projectId={projectId} p={p} />}

      {p.status === 'invalid' && p.problems.length > 0 && (
        <section aria-labelledby="pipeline-problems">
          <h2 id="pipeline-problems" className={styles.sectionTitle}>
            {t('pipelines.problems.title')}
          </h2>
          <ul className={styles.problems}>
            {p.problems.map((pr) => (
              <li key={`${String(pr.line)}:${pr.reason}:${pr.subject ?? ''}`}>
                <Link to={`${blobUrl(project, p.sha, CI_FILE)}${pr.line > 0 ? `#L${String(pr.line)}` : ''}`}>
                  {pr.line > 0
                    ? t('pipelines.problems.line', { line: pr.line })
                    : t('pipelines.problems.file')}
                </Link>{' '}
                <code>{pr.reason}</code>
                {pr.subject !== null && pr.subject !== '' && (
                  <code className={styles.subject}>{pr.subject}</code>
                )}
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

/** One line: why it is where it is, and what to press next (or whom to ask). */
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
        : p.status === 'running'
          ? styles.statusRunning
          : '';
  return (
    <div className={`${styles.statusLine ?? ''} ${tone ?? ''}`} data-testid="pipeline-status-line">
      <p>{statusSentence(t, p, failedJob?.name ?? null, operate)}</p>
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

function statusSentence(t: TFunction, p: Pipeline, job: string | null, operate: boolean): string {
  switch (p.status) {
    case 'pending':
      return operate ? t('pipelines.line.pendingOperate') : t('pipelines.line.pendingAsk');
    case 'running':
      return operate ? t('pipelines.line.runningOperate') : t('pipelines.line.runningAsk');
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
      return t('pipelines.line.error', { id: p.id });
    default:
      return pipelineStatusLabel(t, p.status);
  }
}
