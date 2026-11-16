// "Revert this merge" (M4 C10, plan 04 M-4): on a merged merge request, one
// button opens a merge request that undoes the merge (the branch
// revert/mr-<iid> with one revert commit into the same target). Offered
// only when the server's trial decision says the person may
// (`viewer.can_revert`); the server decides the press again. After it, one
// line says what was made and links to it (plan 08: what happened and what
// to press next). A refusal — not revertible, a revert branch already
// there, or a revert that conflicts (nothing is opened then) — shows its
// user_message and request id, never the code.
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { useMergeStatus, useRevertMergeRequest } from '../../api/mergeRequests';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { pullUrl } from '../repoUrls';
import styles from './Revert.module.css';

/** The revert control with the screen's facts (the status's viewer flag). */
export function RevertSection({
  projectId,
  project,
  iid,
  merged,
}: {
  projectId: number;
  project: string;
  iid: number;
  /** The merge request is merged (nothing to revert otherwise). */
  merged: boolean;
}) {
  const status = useMergeStatus(projectId, iid);
  const canRevert = merged && status.data?.viewer.can_revert === true;
  if (!canRevert) return null;
  return <RevertControl projectId={projectId} project={project} iid={iid} />;
}

export function RevertControl({
  projectId,
  project,
  iid,
}: {
  projectId: number;
  /** The project's path (for the link to the revert). */
  project: string;
  iid: number;
}) {
  const { t } = useTranslation();
  const revert = useRevertMergeRequest(projectId, iid);
  const opened = revert.data?.merge_request.iid ?? null;
  return (
    <section className={styles.revert} aria-labelledby="mr-revert-title" data-testid="mr-revert">
      <h2 id="mr-revert-title" className={styles.title}>
        {t('revert.label')}
      </h2>
      {opened === null ? (
        <>
          <p className={styles.hint}>{t('revert.hint')}</p>
          <div className={styles.row}>
            <button
              type="button"
              className={controls.button}
              aria-disabled={revert.isPending ? 'true' : undefined}
              onClick={() => {
                if (revert.isPending) return;
                revert.mutate();
              }}
            >
              {revert.isPending ? t('revert.pending') : t('revert.button')}
            </button>
          </div>
        </>
      ) : (
        <p role="status" className={styles.done} data-testid="mr-revert-opened">
          {t('revert.opened', { iid: opened })} <Link to={pullUrl(project, opened)}>{t('revert.open')}</Link>
        </p>
      )}
      {revert.isError && <QueryError error={revert.error} />}
    </section>
  );
}
