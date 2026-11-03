// A refused merge request action (approve, withdraw, mark ready, merge): the
// server's user_message and request_id (QueryError), and, for a 409 (the
// merge request changed after it was read: a stale view), a button that
// reads the whole merge request again. The error code is never shown.
import { useTranslation } from 'react-i18next';

import { ApiFailure } from '../../api/client';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { focusWhenNoDialog } from '../../ui/focus';
import { useViewedHead } from './viewedHead';

/** The view the person acted on is out of date (409 on an MR action). */
export function isStale(error: unknown): boolean {
  return error instanceof ApiFailure && error.failure.kind === 'api' && error.failure.status === 409;
}

export function ActionError({
  error,
  onReload,
}: {
  /** The merge request (kept for the callers; the reload is the screen's). */
  projectId: number;
  iid: number;
  error: unknown;
  /** After the reload was asked for (the caller clears its error). */
  onReload?: () => void;
}) {
  const { t } = useTranslation();
  // Reads again only: a newer head brings the "new version" line, not the buttons.
  const { reload } = useViewedHead();
  return (
    <div>
      <QueryError error={error} />
      {isStale(error) && (
        <button
          type="button"
          className={controls.button}
          onClick={() => {
            void reload().then(() => {
              // The button goes away with the error: the conditions take the focus.
              focusWhenNoDialog(() => document.getElementById('mr-conditions-title'));
            });
            onReload?.();
          }}
        >
          {t('mr.actions.reload')}
        </button>
      )}
    </div>
  );
}
