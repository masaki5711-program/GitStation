// The editor's result, shown on the page it led to (the merge request, or the
// file on its branch): the navigation state carries what happened, as
// MovedNotice does for a moved path. A merge request that was asked for but
// could not be opened is said with the server's message and the way to open
// one by hand; the commit and the branch stay.
import { useTranslation } from 'react-i18next';
import { Link, useLocation } from 'react-router';

import type { SingleFileCommitDone } from '../../../api/fileCommits';
import controls from '../../../components/controls.module.css';
import { newPullUrl } from '../../../features/repoUrls';
import styles from './Editor.module.css';

export interface FileSaved {
  kind: 'mr' | 'branch' | 'direct';
  branch: string;
  /** The server's message when the merge request could not be opened. */
  mrError: string | null;
  /** The save's request id (shown with `mrError`). */
  requestId: string | null;
}

/** The navigation state of a saved file. */
export function savedState(result: SingleFileCommitDone): { fileSaved: FileSaved } {
  const mr = result.merge_request ?? null;
  return {
    fileSaved: {
      kind: mr !== null ? 'mr' : result.branch_created ? 'branch' : 'direct',
      branch: result.branch,
      mrError: result.merge_request_error?.message ?? null,
      requestId: result.requestId,
    },
  };
}

function fileSaved(state: unknown): FileSaved | null {
  if (typeof state !== 'object' || state === null || !('fileSaved' in state)) return null;
  const v: unknown = state.fileSaved;
  if (typeof v !== 'object' || v === null) return null;
  if (!('kind' in v) || !('branch' in v) || !('mrError' in v)) return null;
  const { kind, branch, mrError } = v;
  const requestId = 'requestId' in v && typeof v.requestId === 'string' ? v.requestId : null;
  if ((kind !== 'mr' && kind !== 'branch' && kind !== 'direct') || typeof branch !== 'string') return null;
  return { kind, branch, mrError: typeof mrError === 'string' ? mrError : null, requestId };
}

export function SavedNotice({ project }: { project: string }) {
  const { t } = useTranslation();
  const saved = fileSaved(useLocation().state);
  if (saved === null) return null;
  return (
    <div className={saved.mrError === null ? styles.saved : styles.savedWarning} role="status" data-testid="file-saved">
      <p className={styles.savedText}>
        {saved.kind === 'mr'
          ? t('editor.saved.mr', { branch: saved.branch })
          : saved.kind === 'branch'
            ? t('editor.saved.branch', { branch: saved.branch })
            : t('editor.saved.direct', { branch: saved.branch })}
      </p>
      {saved.mrError !== null && (
        <>
          <p className={styles.savedText}>{t('editor.saved.mrFailed', { message: saved.mrError })}</p>
          {saved.requestId !== null && (
            <p className={styles.savedText}>
              {t('error.requestId')}
              {': '}
              <code>{saved.requestId}</code>
            </p>
          )}
          <p className={styles.savedText}>
            <Link className={`${controls.button} ${controls.small}`} to={newPullUrl(project)}>
              {t('editor.saved.openMr')}
            </Link>
          </p>
        </>
      )}
    </div>
  );
}
