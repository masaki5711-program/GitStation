// One job's log (its current attempt), as text only: every page's `text` is
// a React text node in a <pre> (never HTML; terminal colour codes are
// dropped). Pages come one at a time (`next_from`); at most LOG_MAX_PAGES
// and LOG_MAX_CHARS are shown, with a note. While the job runs, the last
// page is read again every few seconds.
import { useTranslation } from 'react-i18next';

import { LOG_MAX_PAGES, isJobMoving, useJobLog, type PipelineJob } from '../../../api/pipelines';
import { Loading } from '../../../components/Loading';
import controls from '../../../components/controls.module.css';
import { RepoError } from '../common';
import styles from './Pipelines.module.css';

/** The characters one view shows; beyond, the start is kept and a note says so. */
export const LOG_MAX_CHARS = 400_000;

// ESC [ … letter: terminal colour / cursor codes (shown as garbage otherwise).
const ANSI = new RegExp(`${String.fromCharCode(27)}\\[[0-9;?]*[A-Za-z]`, 'g');

/** The log text to show: colour codes dropped, cut at `max` characters. */
export function logText(raw: string, max: number = LOG_MAX_CHARS): { text: string; cut: boolean } {
  const clean = raw.replace(ANSI, '');
  return clean.length > max ? { text: clean.slice(0, max), cut: true } : { text: clean, cut: false };
}

export function JobLog({
  projectId,
  pipelineId,
  job,
}: {
  projectId: number;
  pipelineId: number;
  job: PipelineJob;
}) {
  const { t } = useTranslation();
  const log = useJobLog(projectId, pipelineId, job.id);
  const titleId = `job-log-${String(job.id)}`;
  const pages = log.data?.pages ?? [];
  const last = pages[pages.length - 1];
  const { text, cut } = logText(pages.map((p) => p.text).join(''));
  const moving = last !== undefined && !last.complete && isJobMoving(last.job_status);
  const capped = last !== undefined && last.next_from !== null && pages.length >= LOG_MAX_PAGES;
  return (
    <section aria-labelledby={titleId} data-testid="job-log">
      <h2 id={titleId} className={styles.sectionTitle}>
        {t('pipelines.log.title', { job: job.name })}
      </h2>
      {log.isPending && <Loading />}
      {log.isError && (
        <RepoError
          error={log.error}
          onRetry={() => {
            void log.refetch();
          }}
        />
      )}
      {last !== undefined && (
        <>
          {last.attempt > 1 && (
            <p className={styles.hint}>{t('pipelines.log.attempt', { attempt: last.attempt })}</p>
          )}
          {text === '' ? (
            <p className={styles.hint}>{moving ? t('pipelines.log.waiting') : t('pipelines.log.empty')}</p>
          ) : (
            <pre className={styles.log} data-testid="job-log-text" tabIndex={0} aria-labelledby={titleId}>
              {text}
            </pre>
          )}
          {(cut || capped) && <p className={styles.hint}>{t('pipelines.log.truncated')}</p>}
          {moving && <p className={styles.hint}>{t('pipelines.log.live')}</p>}
          {log.hasNextPage && !cut && (
            <button
              type="button"
              className={controls.button}
              disabled={log.isFetchingNextPage}
              onClick={() => {
                void log.fetchNextPage();
              }}
            >
              {t('pipelines.log.more')}
            </button>
          )}
        </>
      )}
    </section>
  );
}
