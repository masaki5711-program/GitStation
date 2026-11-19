// "Merge when ready" (M4 C8, ADR-0030): on the merge request screen, a
// button that puts the merge request in its target's merge queue, and the
// queue's word on it — its place, "being tested", merged, or why it left
// (plan 08: one line saying why it stopped and what to press next). The
// queue merges for the person who asked once a pipeline on it (with what is
// ahead of it) passes. The server decides every press again; a refusal shows
// its user_message and request id (never the code).
//
// Plan 18 S5: on a target that requires the merge request pipeline
// (`ci/pipeline`) a target move starts no pipeline of its own, so this is
// THE merge button there — "Merge when ready", primary, with the direct
// "Merge" not offered — and the run of the pipeline on the current target
// sits beside it (PipelineRun).
import type { TFunction } from 'i18next';
import { useId } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { useMergeStatus } from '../../api/mergeRequests';
import { isLive, useDequeue, useEnqueue, useQueueEntry, type QueueEntry } from '../../api/mergeQueue';
import controls from '../../components/controls.module.css';
import { encodePath } from '../repoUrls';
import { ActionError } from './ActionError';
import { requiresMergeResultPipeline } from './blockedReason';
import styles from './Mr.module.css';
import { PipelineRunControl } from './PipelineRun';
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
  const data = status.data;
  // Offered only when nothing but what the queue proves itself blocks the
  // merge (the merge result's `ci/pipeline`); the server decides again.
  const queueable =
    data !== undefined &&
    data.viewer.can_merge &&
    data.reasons.every((r) => queueProves(r.reason, r.detail));
  return (
    <>
      <QueueControl
        projectId={projectId}
        project={project}
        iid={iid}
        open={open}
        canMerge={queueable}
        mergeWhenReady={data !== undefined && requiresMergeResultPipeline(data)}
        viewedHead={viewed.known ? viewed.head : null}
      />
      {data !== undefined && (
        <PipelineRunControl projectId={projectId} iid={iid} status={data} open={open} />
      )}
    </>
  );
}

export function QueueControl({
  projectId,
  project,
  iid,
  open,
  canMerge,
  mergeWhenReady = false,
  viewedHead,
}: {
  projectId: number;
  /** The project's path (for the queue page link). */
  project: string;
  iid: number;
  open: boolean;
  /** The server's trial decision of a merge (the queue is decided alike). */
  canMerge: boolean;
  /** Plan 18 S5: the target requires the merge request pipeline — the
   * queue is the merge ("Merge when ready", the primary button). */
  mergeWhenReady?: boolean;
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
  const hintId = useId();
  const showHint = mayEnqueue && current === null;

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
              className={mergeWhenReady ? controls.primary : controls.button}
              aria-disabled={busy ? 'true' : undefined}
              aria-describedby={showHint ? hintId : undefined}
              onClick={() => {
                if (busy) return;
                dequeue.reset();
                enqueue.mutate(viewedHead);
              }}
            >
              {mergeWhenReady ? t('mergeQueue.mergeWhenReady') : t('mergeQueue.enqueue')}
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
      {showHint && (
        <p id={hintId} className={styles.nextHint}>
          {mergeWhenReady ? t('mergeQueue.mergeWhenReadyHint') : t('mergeQueue.enqueueHint')}
        </p>
      )}
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

/** A blocking reason the queue's own train pipeline settles: a missing,
 * stale or still running `ci/pipeline` of the merge result. */
export function queueProves(reason: string, detail: string | null): boolean {
  const ciPipeline = detail?.split(' ').includes('name=ci/pipeline') === true;
  return (
    ciPipeline &&
    (reason === 'required_check_missing' ||
      reason === 'required_check_stale' ||
      reason === 'required_check_not_successful')
  );
}

/** One line: where the entry is, or why it left and what to do next. */
export function entryLine(t: TFunction, e: QueueEntry): string {
  switch (e.state) {
    case 'queued':
      return t('mergeQueue.state.queued', { position: e.position ?? 1 });
    case 'testing':
      return t('mergeQueue.state.testing');
    case 'merged':
      return t('mergeQueue.state.merged');
    default:
      return t('mergeQueue.state.removed', {
        why: reasonLabel(t, e.reason),
        next: reasonNext(t, e.reason),
      });
  }
}

/** The reason a queue removed an entry, in words (the code is never shown). */
export function reasonLabel(t: TFunction, reason: string | null): string {
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
    // The train's document lint (plan 18 S1): whose problem it is, in words.
    case 'doc_lint_failed':
      return t('mergeQueue.reason.doc_lint_failed');
    case 'doc_lint_base_not_clean':
      return t('mergeQueue.reason.doc_lint_base_not_clean');
    case 'doc_lint_unavailable':
      return t('mergeQueue.reason.doc_lint_unavailable');
    case 'doc_lint_repo_config_invalid':
      return t('mergeQueue.reason.doc_lint_repo_config_invalid');
    default:
      return t('mergeQueue.reason.other');
  }
}

/**
 * What to do after the queue removed an entry. "Fix it, then add it again"
 * is wrong when the change is not the cause (the target's documents, the
 * repository's config, a check that could not run), so those say who acts.
 */
export function reasonNext(t: TFunction, reason: string | null): string {
  switch (reason) {
    case 'doc_lint_failed':
      return t('mergeQueue.next.doc_lint_failed');
    case 'doc_lint_base_not_clean':
      return t('mergeQueue.next.doc_lint_base_not_clean');
    case 'doc_lint_unavailable':
      return t('mergeQueue.next.doc_lint_unavailable');
    case 'doc_lint_repo_config_invalid':
      return t('mergeQueue.next.doc_lint_repo_config_invalid');
    default:
      return t('mergeQueue.next.fix');
  }
}
