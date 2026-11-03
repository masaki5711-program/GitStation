// A refused merge request action (approve, withdraw, mark ready, merge): the
// server's user_message and request_id (QueryError), and, for a 409 (the
// merge request changed after it was read: a stale view), a button that
// reads the whole merge request again. The error code is never shown.
import { useTranslation } from 'react-i18next';

import { ApiFailure } from '../../api/client';
import { useReloadMergeRequest } from '../../api/mergeRequests';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';

/** The view the person acted on is out of date (409 on an MR action). */
export function isStale(error: unknown): boolean {
  return error instanceof ApiFailure && error.failure.kind === 'api' && error.failure.status === 409;
}

export function ActionError({
  projectId,
  iid,
  error,
  onReload,
}: {
  projectId: number;
  iid: number;
  error: unknown;
  /** After the reload was asked for (the caller clears its error). */
  onReload?: () => void;
}) {
  const { t } = useTranslation();
  const reload = useReloadMergeRequest(projectId, iid);
  return (
    <div>
      <QueryError error={error} />
      {isStale(error) && (
        <button
          type="button"
          className={controls.button}
          onClick={() => {
            void reload();
            onReload?.();
          }}
        >
          {t('mr.actions.reload')}
        </button>
      )}
    </div>
  );
}
