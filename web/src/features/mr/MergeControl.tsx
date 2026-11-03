// Merge (U2): one confirmation dialog that is also the method choice (only
// the project's allowed methods, its default selected, plus "delete the
// source branch"), then the attempt is followed in a polite live region until
// it ends: applied (and what happened to the source branch), superseded (one
// line per reason) or failed, each with the request id of the merge request.
// After 2 minutes the screen stops asking and says the merge goes on in the
// server. The attempt's error_code is never shown (CLAUDE.md contract 3).
//
// The merge names the head of the version on screen (`sha`, from the
// versions the Changes tab renders), captured when the dialog opens: a push
// after that is refused by the server (409) instead of merged unseen.
import type { TFunction } from 'i18next';
import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import {
  MERGE_POLL_GIVE_UP_MS,
  isFinished,
  useMergeAttempt,
  useStartMerge,
} from '../../api/mergeRequests';
import { isMergeMethod, useMergeSettings } from '../../api/mergeSettings';
import type { MergeAttempt, MergeStatus } from '../../api/types';
import { RadioField } from '../../components/Field';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { Modal } from '../../ui/Modal';
import { focusWhenNoDialog } from '../../ui/focus';
import { mergeMethodHelp, mergeMethodLabel } from '../labels';
import { ActionError } from './ActionError';
import styles from './Mr.module.css';

export function MergeButton({
  projectId,
  iid,
  status,
  viewedHead,
  sourceBranch,
  onStarted,
}: {
  projectId: number;
  iid: number;
  status: MergeStatus;
  /** The head of the version on screen (null: none read yet). */
  viewedHead: string | null;
  sourceBranch: string;
  /** The merge was queued (202): follow it, from this attempt on. */
  onStarted: (mergeId: number) => void;
}) {
  const { t } = useTranslation();
  /** The head captured when the dialog opened; null while it is closed. */
  const [openSha, setOpenSha] = useState<string | null>(null);
  const open = openSha !== null;
  const button = useRef<HTMLButtonElement>(null);
  const sending = useRef(false);
  // Shown only when the server said a merge would be allowed, nothing blocks
  // it now and the version on screen is the head the status is about; the
  // server decides again (and the job under its lock).
  const shown =
    status.viewer.can_merge && status.mergeable && viewedHead !== null && viewedHead === status.head_sha;
  const settings = useMergeSettings(projectId, shown || open);
  const start = useStartMerge(projectId, iid);
  const [method, setMethod] = useState<string | null>(null);
  const [deleteSource, setDeleteSource] = useState<boolean | null>(null);
  // An open dialog stays (its request names the head captured at opening).
  if (!shown && !open) return null;

  const allowed = settings.data?.allowed_methods.filter(isMergeMethod) ?? [];
  const chosen =
    method !== null && allowed.some((m) => m === method)
      ? method
      : (settings.data?.default_method ?? allowed[0] ?? null);
  const deleting = deleteSource ?? settings.data?.delete_source_default ?? false;

  const close = () => {
    setOpenSha(null);
    focusWhenNoDialog(button);
  };
  const confirm = () => {
    // A second click before the first answer must not ask twice.
    if (sending.current || start.isPending || chosen === null || openSha === null) return;
    sending.current = true;
    start.mutate(
      { sha: openSha, method: chosen, deleteSourceBranch: deleting },
      {
        onSuccess: (attempt) => {
          setOpenSha(null);
          onStarted(attempt.merge_id);
        },
        onSettled: () => {
          sending.current = false;
        },
      },
    );
  };

  return (
    <>
      <button
        ref={button}
        type="button"
        className={controls.primary}
        onClick={() => {
          if (viewedHead === null) return;
          start.reset();
          setMethod(null);
          setDeleteSource(null);
          setOpenSha(viewedHead);
        }}
      >
        {t('mr.merge.open')}
      </button>
      <Modal open={open} onClose={close} title={t('mr.merge.title')} busy={start.isPending}>
        <div data-testid="merge-dialog">
          <p>{t('mr.merge.explain', { source: sourceBranch, target: status.target_branch })}</p>
          {settings.isPending && <Loading />}
          {settings.isError && (
            <QueryError
              error={settings.error}
              onRetry={() => {
                void settings.refetch();
              }}
            />
          )}
          {settings.isSuccess && chosen !== null && (
            <>
              <RadioField<string>
                legend={t('mr.merge.methodLegend')}
                name="merge-method"
                value={chosen}
                onChange={setMethod}
                choices={allowed.map((m) => ({
                  value: m,
                  label: mergeMethodLabel(t, m),
                  help: mergeMethodHelp(t, m),
                }))}
              />
              <div className={controls.field}>
                <label className={controls.choiceLabel}>
                  <input
                    type="checkbox"
                    checked={deleting}
                    onChange={(e) => {
                      setDeleteSource(e.target.checked);
                    }}
                  />
                  <span>{t('mr.merge.deleteSource', { source: sourceBranch })}</span>
                </label>
              </div>
            </>
          )}
          {start.isError && (
            <ActionError
              projectId={projectId}
              iid={iid}
              error={start.error}
              onReload={() => {
                start.reset();
                close();
              }}
            />
          )}
          <div className={styles.actions}>
            <button type="button" className={controls.button} disabled={start.isPending} onClick={close}>
              {t('common.cancel')}
            </button>
            <button
              type="button"
              className={controls.primary}
              aria-disabled={start.isPending || chosen === null ? 'true' : undefined}
              onClick={confirm}
            >
              {start.isPending ? t('mr.merge.starting') : t('mr.merge.confirm')}
            </button>
          </div>
        </div>
      </Modal>
    </>
  );
}

function sourceLine(t: TFunction, outcome: string | null): string | null {
  switch (outcome) {
    case 'deleted':
      return t('mr.merge.source.deleted');
    case 'kept_refused':
      return t('mr.merge.source.keptRefused');
    case 'kept_moved':
      return t('mr.merge.source.keptMoved');
    case 'kept_failed':
      return t('mr.merge.source.keptFailed');
    default:
      return null;
  }
}

function supersededLine(t: TFunction, reason: string | null): string {
  switch (reason) {
    case 'target_moved':
      return t('mr.merge.superseded.targetMoved');
    case 'conflict':
      return t('mr.merge.superseded.conflict');
    case 'job_failed':
      return t('mr.merge.superseded.jobFailed');
    case 'method_not_allowed':
      return t('mr.merge.superseded.methodNotAllowed');
    case 'source_moved':
      return t('mr.merge.superseded.sourceMoved');
    default:
      return t('mr.merge.superseded.other');
  }
}

/** The words for an attempt, and whether its request id is worth showing. */
function attemptLines(t: TFunction, a: MergeAttempt, gaveUp: boolean): { lines: string[]; withId: boolean } {
  switch (a.state) {
    case 'applied': {
      const source = sourceLine(t, a.source_outcome);
      return {
        lines: [t('mr.merge.applied', { target: a.target_branch }), ...(source === null ? [] : [source])],
        withId: false,
      };
    }
    case 'superseded':
      return { lines: [supersededLine(t, a.reason)], withId: true };
    case 'failed':
      return { lines: [t('mr.merge.failed')], withId: true };
    default:
      if (gaveUp) return { lines: [t('mr.merge.stillRunning')], withId: true };
      return {
        lines: [
          a.state === 'queued'
            ? t('mr.merge.queued')
            : a.state === 'prepared'
              ? t('mr.merge.prepared')
              : t('mr.merge.executing'),
        ],
        withId: false,
      };
  }
}

/** Follows the latest attempt from `since` (Date.now() when watching began). */
export function MergeProgress({
  projectId,
  iid,
  since,
  minMergeId,
  focusOnStart,
}: {
  projectId: number;
  iid: number;
  since: number | null;
  /** The attempt this screen started: an older one in the cache is not it. */
  minMergeId: number;
  /** Take the focus when the dialog that started the merge has closed. */
  focusOnStart: boolean;
}) {
  const { t } = useTranslation();
  const latest = useMergeAttempt(projectId, iid, since, minMergeId);
  const region = useRef<HTMLDivElement>(null);
  const [gaveUp, setGaveUp] = useState(false);

  useEffect(() => {
    setGaveUp(false);
    if (since === null) return;
    const timer = window.setTimeout(
      () => {
        setGaveUp(true);
      },
      Math.max(0, since + MERGE_POLL_GIVE_UP_MS - Date.now()),
    );
    return () => {
      window.clearTimeout(timer);
    };
  }, [since]);

  useEffect(() => {
    if (since !== null && focusOnStart) focusWhenNoDialog(region);
  }, [since, focusOnStart]);

  const cached = latest.data?.merge ?? null;
  const a = since === null || cached === null || cached.merge_id < minMergeId ? null : cached;
  const shown = a === null ? null : attemptLines(t, a, gaveUp && !isFinished(a));
  return (
    // One region that stays mounted: each new state is a change, so it is read out.
    <div
      ref={region}
      tabIndex={-1}
      role="status"
      aria-live="polite"
      aria-label={t('mr.merge.progressLabel')}
      className={styles.progress}
      data-testid="mr-merge-progress"
    >
      {shown?.lines.map((line) => (
        <p key={line} className={styles.progressLine}>
          {line}
        </p>
      ))}
      {shown?.withId === true && a !== null && (
        <p className={styles.nextHint}>
          {t('error.requestId')}
          {': '}
          <code>{a.request_id}</code>
        </p>
      )}
      {latest.isError && since !== null && <QueryError error={latest.error} />}
    </div>
  );
}
