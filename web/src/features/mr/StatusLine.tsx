// "Why it is stopped, and what to do next" in one line (plan 08 §1.7): the
// first reason of the status contract through blockedReason(). The line is a
// polite live region, so a change (a thread resolved, the check finished) is
// announced. A failed status read is shown here only, with its request_id;
// the rest of the page does not depend on it.
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { useMergeStatus } from '../../api/mergeRequests';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { pullUrl } from '../repoUrls';
import { blockedReason, nextKey, statusHint, statusTone, type StatusTone } from './blockedReason';
import styles from './Mr.module.css';

/** Router state of a link that asks the Overview to focus the first
 * unresolved thread (or, when the threads could not be read, their section). */
export const FOCUS_FIRST_UNRESOLVED = { focus: 'first-unresolved' } as const;

export function StatusLine({
  projectId,
  project,
  iid,
  onFirstUnresolved,
}: {
  projectId: number;
  project: string;
  iid: number;
  /** Focus the first unresolved thread on this page; `null`: not here (the
   * Changes tab, or the threads could not be read), so "next" is a link to
   * the Overview, which focuses it (or the threads' error) on arrival. */
  onFirstUnresolved: (() => void) | null;
}) {
  const { t } = useTranslation();
  const status = useMergeStatus(projectId, iid);

  // One live region that stays mounted: its text changes, so each change is
  // announced (a region that appears with its text is often not).
  let text = '';
  let next = null;
  let hint: string | null = null;
  let tone: StatusTone = 'info';
  if (status.isPending) text = t('mr.status.checking');
  if (status.isSuccess) {
    const reason = blockedReason(status.data);
    text = t(reason.key, reason.values);
    tone = statusTone(status.data);
    const hintKey = statusHint(status.data);
    hint = hintKey === null ? null : t(hintKey);
    const word = nextKey(reason.next);
    const label = word === null ? null : t(word);
    if (label !== null && reason.next === 'changes') {
      next = <Link to={pullUrl(project, iid, 'changes')}>{label}</Link>;
    } else if (label !== null && reason.next === 'first_unresolved') {
      next =
        onFirstUnresolved === null ? (
          <Link to={pullUrl(project, iid)} state={FOCUS_FIRST_UNRESOLVED}>
            {label}
          </Link>
        ) : (
          <button type="button" className={controls.button} onClick={onFirstUnresolved}>
            {label}
          </button>
        );
    } else if (label !== null) {
      next = <span className={styles.nextHint}>{label}</span>;
    }
  }
  const toneClass =
    tone === 'ready' ? styles.statusReady : tone === 'blocked' ? styles.statusBlocked : styles.statusText;
  return (
    <div className={styles.statusArea} data-testid="mr-status-area">
      <div className={styles.statusRow}>
        <p
          role="status"
          className={toneClass}
          data-testid="mr-status"
          data-tone={tone}
        >
          {text}
        </p>
        {next}
        {hint !== null && <span className={styles.nextHint}>{hint}</span>}
      </div>
      {status.isError && (
        <QueryError
          error={status.error}
          onRetry={() => {
            void status.refetch();
          }}
        />
      )}
    </div>
  );
}
