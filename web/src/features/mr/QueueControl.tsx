// "Merge when ready" (M4 C8, ADR-0030): on the merge request screen, a
// button that puts the merge request in its target's merge queue, and the
// queue's word on it — its place, "being tested", merged, or why it left
// (plan 08: one line saying why it stopped and what to press next). The
// queue merges for the person who asked once a pipeline on it (with what is
// ahead of it) passes. The server decides every press again; a refusal shows
// its user_message and request id (never the code).
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { useMergeStatus } from '../../api/mergeRequests';
import { isLive, useDequeue, useEnqueue, useQueueEntry, type QueueEntry } from '../../api/mergeQueue';
import controls from '../../components/controls.module.css';
import { encodePath } from '../repoUrls';
import { ActionError } from './ActionError';
import styles from './Mr.module.css';
import { useViewedHead } from './viewedHead';

/** The queue control with the screen's facts: the server's trial decision
 * of a merge and the head of the version on screen. */
export function QueueSection({
  projectId,
  project,
  iid,
  open,
}: {
  projectId: number;
  project: string;
  iid: number;
  open: boolean;
}) {
  const status = useMergeStatus(projectId, iid);
  const viewed = useViewedHead();
  return (
    <QueueControl
      projectId={projectId}
      project={project}
      iid={iid}
      open={open}
      canMerge={status.data?.viewer.can_merge === true}
      viewedHead={viewed.known ? viewed.head : null}
    />
  );
}

export function QueueControl({
  projectId,
  project,
  iid,
  open,
  canMerge,
  viewedHead,
}: {
  projectId: number;
  /** The project's path (for the queue page link). */
  project: string;
  iid: number;
  open: boolean;
  /** The server's trial decision of a merge (the queue is decided alike). */
  canMerge: boolean;
  /** The head of the version on screen (null: none read yet). */
  viewedHead: string | null;
}) {
  const { t } = useTranslation();
  const entry = useQueueEntry(projectId, iid);
  const enqueue = useEnqueue(projectId, iid);
  const dequeue = useDequeue(projectId, iid);
  const current = entry.data?.entry ?? null;
  const live = current !== null && isLive(current.state);
  const busy = enqueue.isPending || dequeue.isPending;
  const failed = enqueue.isError ? enqueue.error : dequeue.isError ? dequeue.error : null;
  const mayEnqueue = open && canMerge && viewedHead !== null && !live;
  const mayDequeue = canMerge && live;

  return (
    <section className={styles.queue} aria-label={t('mergeQueue.mrLabel')} data-testid="mr-queue">
      {current !== null && (
        <p role="status" className={styles.nextHint} data-testid="mr-queue-status">
          {entryLine(t, current)}{' '}
          {current.target_branch !== '' && (
            <Link
              to={`/${encodePath(project)}/-/merge-queue?target=${encodeURIComponent(current.target_branch)}`}
            >
              {t('mergeQueue.openQueue')}
            </Link>
          )}
        </p>
      )}
      {(mayEnqueue || mayDequeue) && (
        <div className={styles.actions} role="group" aria-label={t('mergeQueue.actionsLabel')}>
          {mayEnqueue && (
            <button
              type="button"
              className={controls.button}
              aria-disabled={busy ? 'true' : undefined}
              onClick={() => {
                if (busy || viewedHead === null) return;
                dequeue.reset();
                enqueue.mutate(viewedHead);
              }}
            >
              {t('mergeQueue.enqueue')}
            </button>
          )}
          {mayDequeue && (
            <button
              type="button"
              className={controls.button}
              aria-disabled={busy ? 'true' : undefined}
              onClick={() => {
                if (busy) return;
                enqueue.reset();
                dequeue.mutate();
              }}
            >
              {t('mergeQueue.dequeue')}
            </button>
          )}
        </div>
      )}
      {mayEnqueue && current === null && <p className={styles.nextHint}>{t('mergeQueue.enqueueHint')}</p>}
      {failed !== null && (
        <ActionError
          projectId={projectId}
          iid={iid}
          error={failed}
          onReload={() => {
            enqueue.reset();
            dequeue.reset();
          }}
        />
      )}
    </section>
  );
}

/** One line: where the entry is, or why it left and what to do next. */
export function entryLine(t: (key: string, opts?: Record<string, unknown>) => string, e: QueueEntry): string {
  switch (e.state) {
    case 'queued':
      return t('mergeQueue.state.queued', { position: e.position ?? 1 });
    case 'testing':
      return t('mergeQueue.state.testing');
    case 'merged':
      return t('mergeQueue.state.merged');
    default:
      return t('mergeQueue.state.removed', { why: reasonLabel(t, e.reason) });
  }
}

/** The reason a queue removed an entry, in words (the code is never shown). */
export function reasonLabel(t: (key: string) => string, reason: string | null): string {
  switch (reason) {
    case 'dequeued':
      return t('mergeQueue.reason.dequeued');
    case 'checks_failed':
      return t('mergeQueue.reason.checks_failed');
    case 'conflict':
      return t('mergeQueue.reason.conflict');
    case 'head_changed':
      return t('mergeQueue.reason.head_changed');
    case 'stalled':
      return t('mergeQueue.reason.stalled');
    case 'not_open':
      return t('mergeQueue.reason.not_open');
    default:
      return t('mergeQueue.reason.other');
  }
}
