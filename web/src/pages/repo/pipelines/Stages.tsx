// A pipeline's jobs as a simple list per stage, stages in the order their
// first job runs (wave, then id); each job's status, attempt, needs, time
// taken (the moments in its title) and a link to its log.
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import type { PipelineJob } from '../../../api/pipelines';
import { formatDuration, formatSpan } from '../../../features/pipelineLabels';
import { pipelineUrl } from '../../../features/repoUrls';
import { PipelineBadge } from './PipelineBadge';
import styles from './Pipelines.module.css';

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

export function Stages({
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
  const { t, i18n } = useTranslation();
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
                    <PipelineBadge status={j.status} />
                    <code>{j.name}</code>
                  </span>
                  <span className={styles.meta}>
                    {j.attempt > 1 && <span>{t('pipelines.jobs.attempt', { attempt: j.attempt })}</span>}
                    {j.needs.length > 0 && (
                      <span>{t('pipelines.jobs.needs', { needs: j.needs.join(', ') })}</span>
                    )}
                    {duration !== null && (
                      <span
                        className={styles.numeric}
                        title={formatSpan(t, j.started_at, j.finished_at, i18n.language) ?? undefined}
                      >
                        {t('pipelines.took', { duration })}
                      </span>
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
