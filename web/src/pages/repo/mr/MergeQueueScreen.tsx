// A target branch's merge queue (M4 C8, ADR-0030): `/-/merge-queue?target=`.
// Whether it runs or is stopped (and the button to stop / start it, shown
// from the server's trial decision), the batch being tested, the merge
// requests waiting in order, and the latest ones that left with the reason
// in words (plan 08: why it stopped and what to press next). Read again every
// few seconds while something waits.
import { useTranslation } from 'react-i18next';
import { Link, useSearchParams } from 'react-router';

import { useMergeQueue, useSetPaused, type QueueEntry } from '../../../api/mergeQueue';
import { Loading } from '../../../components/Loading';
import { QueryError } from '../../../components/QueryError';
import controls from '../../../components/controls.module.css';
import { entryLine } from '../../../features/mr/QueueControl';
import { pipelinesUrl, pullUrl } from '../../../features/repoUrls';
import { useDocumentTitle } from '../../../hooks/useDocumentTitle';
import { EmptyState } from '../../../ui/EmptyState';
import { PageHeader } from '../../../ui/PageHeader';
import surfaces from '../../../ui/surfaces.module.css';
import { RepoError } from '../common';
import styles from './Mr.module.css';

export function MergeQueueScreen({ projectId, project }: { projectId: number; project: string }) {
  const { t } = useTranslation();
  const [params] = useSearchParams();
  const target = params.get('target') ?? 'main';
  const queue = useMergeQueue(projectId, target);
  const toggle = useSetPaused(projectId, target);
  const title = t('mergeQueue.title', { target });
  useDocumentTitle(title);
  const data = queue.data;
  const canToggle = data !== undefined && (data.paused ? data.viewer.can_resume : data.viewer.can_pause);

  return (
    <>
      <PageHeader
        title={title}
        subtitle={t('mergeQueue.subtitle')}
        actions={
          canToggle ? (
            <button
              type="button"
              className={controls.button}
              aria-disabled={toggle.isPending ? 'true' : undefined}
              onClick={() => {
                if (toggle.isPending) return;
                toggle.mutate(!data.paused);
              }}
            >
              {data.paused ? t('mergeQueue.resume') : t('mergeQueue.pause')}
            </button>
          ) : undefined
        }
      />
      {toggle.isError && <QueryError error={toggle.error} />}
      {queue.isPending && <Loading />}
      {queue.isError && (
        <RepoError
          error={queue.error}
          onRetry={() => {
            void queue.refetch();
          }}
        />
      )}
      {data !== undefined && (
        <>
          <p role="status" className={styles.queueState} data-testid="queue-state">
            {data.paused ? t('mergeQueue.paused') : t('mergeQueue.running', { limit: data.batch_limit })}
          </p>
          {data.batch !== null && (
            <section className={surfaces.card} aria-labelledby="queue-batch-title">
              <h2 id="queue-batch-title" className={styles.queueHeading}>
                {t('mergeQueue.batchTitle')}
              </h2>
              <p>
                {t('mergeQueue.batchLine', {
                  size: data.batch.size,
                  train: data.batch.train_sha.slice(0, 8),
                })}{' '}
                <Link to={pipelinesUrl(project)}>{t('mergeQueue.openPipelines')}</Link>
              </p>
            </section>
          )}
          {data.entries.length === 0 ? (
            <EmptyState
              icon="mergeRequest"
              reason={t('mergeQueue.empty')}
              hints={[t('mergeQueue.emptyHint')]}
            />
          ) : (
            <EntryList project={project} items={data.entries} label={t('mergeQueue.listLabel')} />
          )}
          {data.recent.length > 0 && (
            <section aria-labelledby="queue-recent-title">
              <h2 id="queue-recent-title" className={styles.queueHeading}>
                {t('mergeQueue.recentTitle')}
              </h2>
              <EntryList project={project} items={data.recent} label={t('mergeQueue.recentTitle')} />
            </section>
          )}
        </>
      )}
    </>
  );
}

function EntryList({ project, items, label }: { project: string; items: QueueEntry[]; label: string }) {
  const { t } = useTranslation();
  return (
    <ol className={styles.queueList} aria-label={label} data-testid="queue-entries">
      {items.map((e) => (
        <li key={e.entry_id} className={surfaces.card}>
          <Link to={pullUrl(project, e.iid)}>{t('mergeQueue.mr', { iid: e.iid })}</Link>{' '}
          <span>{entryLine(t, e)}</span>
          {e.isolate !== null && <span className={styles.queueNote}> {t('mergeQueue.alone')}</span>}
        </li>
      ))}
    </ol>
  );
}
