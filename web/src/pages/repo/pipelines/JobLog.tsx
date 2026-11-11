// One job's log (its current attempt), as text only: the pages' text is a
// React text node in a <pre> (never HTML; terminal colour codes are dropped
// page by page, a code cut at a page's end carried to the next). Pages come
// one at a time (`next_from`); at most LOG_MAX_PAGES and LOG_MAX_CHARS are
// shown, with a note. While the job runs, only the last page is read again.
// Opening a log (the address's `?job=`) moves the focus to its heading.
import { useEffect, useMemo, useRef } from 'react';
import { useTranslation } from 'react-i18next';

import { isJobMoving, useJobLog, type PipelineJob } from '../../../api/pipelines';
import { Loading } from '../../../components/Loading';
import controls from '../../../components/controls.module.css';
import { RepoError } from '../common';
import styles from './Pipelines.module.css';

/** The characters one view shows; beyond, the start is kept and a note says so. */
export const LOG_MAX_CHARS = 400_000;

const ESC = String.fromCharCode(27);
// ESC [ … letter: terminal colour / cursor codes (shown as garbage otherwise).
const ANSI = new RegExp(`${ESC}\\[[0-9;?]*[A-Za-z]`, 'g');
// A code not finished at the end of a page: ESC, or ESC [ and parameters.
const PARTIAL = new RegExp(`${ESC}(\\[[0-9;?]*)?$`);

/** One page's text without colour codes; `carry` is the unfinished code the
 * page before ended with, and the result's `carry` this page's. */
export function cleanPage(raw: string, carry = ''): { text: string; carry: string } {
  const joined = carry + raw;
  const m = PARTIAL.exec(joined);
  const whole = m === null ? joined : joined.slice(0, m.index);
  return { text: whole.replace(ANSI, ''), carry: m === null ? '' : m[0] };
}

/** Pages' texts cleaned in order (a code may span two pages). */
export function cleanPages(raws: readonly string[]): { texts: string[]; carry: string } {
  const texts: string[] = [];
  let carry = '';
  for (const raw of raws) {
    const page = cleanPage(raw, carry);
    texts.push(page.text);
    carry = page.carry;
  }
  return { texts, carry };
}

/** The text to show: cut at `max` characters. */
export function logText(raw: string, max: number = LOG_MAX_CHARS): { text: string; cut: boolean } {
  const clean = cleanPage(raw).text;
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
  const heading = useRef<HTMLHeadingElement>(null);
  const titleId = `job-log-${String(job.id)}`;

  // Keyed by the job: mounting is "this log was opened".
  useEffect(() => {
    const el = heading.current;
    if (el === null) return;
    el.focus();
    if (typeof el.scrollIntoView === 'function') el.scrollIntoView({ block: 'start' });
  }, []);

  // The finished pages change only when one is appended; the last one on every poll.
  const before = useMemo(() => cleanPages(log.done.map((p) => p.text)), [log.done]);
  const last = log.tail.data;
  const lastText = useMemo(
    () => (last === undefined ? '' : cleanPage(last.text, before.carry).text),
    [last, before.carry],
  );
  const { text, cut } = logText([...before.texts, lastText].join(''));
  const shown = last ?? log.done[log.done.length - 1];
  const moving = last !== undefined && !last.complete && isJobMoving(last.job_status);

  return (
    <section aria-labelledby={titleId} data-testid="job-log">
      <h2 id={titleId} ref={heading} tabIndex={-1} className={styles.sectionTitle}>
        {t('pipelines.log.title', { job: job.name })}
      </h2>
      {log.tail.isPending && log.done.length === 0 && <Loading />}
      {log.tail.isError && (
        <RepoError
          error={log.tail.error}
          onRetry={() => {
            void log.tail.refetch();
          }}
        />
      )}
      {shown !== undefined && (
        <>
          {shown.attempt > 1 && (
            <p className={styles.hint}>{t('pipelines.log.attempt', { attempt: shown.attempt })}</p>
          )}
          {text === '' ? (
            <p className={styles.hint}>{moving ? t('pipelines.log.waiting') : t('pipelines.log.empty')}</p>
          ) : (
            <pre className={styles.log} data-testid="job-log-text" tabIndex={0} aria-labelledby={titleId}>
              {text}
            </pre>
          )}
          {log.tail.isPending && <Loading />}
          {(cut || log.capped) && <p className={styles.hint}>{t('pipelines.log.truncated')}</p>}
          {moving && <p className={styles.hint}>{t('pipelines.log.live')}</p>}
          {log.hasMore && !cut && (
            <button type="button" className={controls.button} onClick={log.more}>
              {t('pipelines.log.more')}
            </button>
          )}
        </>
      )}
    </section>
  );
}
