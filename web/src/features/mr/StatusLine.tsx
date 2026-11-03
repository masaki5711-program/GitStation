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
import { blockedReason, nextKey } from './blockedReason';
import styles from './Mr.module.css';

export function StatusLine({
  projectId,
  project,
  iid,
  onFirstUnresolved,
}: {
  projectId: number;
  project: string;
  iid: number;
  /** Focus the first unresolved thread; `null` while there is none to focus. */
  onFirstUnresolved: (() => void) | null;
}) {
  const { t } = useTranslation();
  const status = useMergeStatus(projectId, iid);

  // One live region that stays mounted: its text changes, so each change is
  // announced (a region that appears with its text is often not).
  let text = '';
  let next = null;
  if (status.isPending) text = t('mr.status.checking');
  if (status.isSuccess) {
    const reason = blockedReason(status.data);
    text = t(reason.key, reason.values);
    const word = nextKey(reason.next);
    const label = word === null ? null : t(word);
    if (label !== null && reason.next === 'changes') {
      next = <Link to={pullUrl(project, iid, 'changes')}>{label}</Link>;
    } else if (label !== null && reason.next === 'first_unresolved') {
      next =
        onFirstUnresolved === null ? null : (
          <button type="button" className={controls.button} onClick={onFirstUnresolved}>
            {label}
          </button>
        );
    } else if (label !== null) {
      next = <span className={styles.nextHint}>{label}</span>;
    }
  }
  const ready = status.isSuccess && status.data.mergeable;
  return (
    <div className={styles.statusArea} data-testid="mr-status-area">
      <div className={styles.statusRow}>
        <p
          role="status"
          className={ready ? styles.statusReady : styles.statusText}
          data-testid="mr-status"
        >
          {text}
        </p>
        {next}
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
