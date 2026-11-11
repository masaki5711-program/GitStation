// A failed query in a small place (the sidebar, a side panel): the server's
// `user_message`, the `request_id` and a retry, on a few lines (contract 3:
// never the error code or a status). Not a live region: the page's own error
// display announces a failure; this one stays where the missing part is.
// `hidden`: the text is visually hidden (a collapsed sidebar); the retry
// stays a visible icon button with its name. No retry for 403 or an ended
// session (as QueryError).
import { useEffect } from 'react';
import { useTranslation } from 'react-i18next';

import { ApiFailure, failureOf } from '../api/client';
import styles from './InlineError.module.css';

export function InlineError({
  error,
  onRetry,
  hidden = false,
}: {
  error: unknown;
  onRetry: () => void;
  hidden?: boolean;
}) {
  const { t } = useTranslation();
  useEffect(() => {
    if (!(error instanceof ApiFailure)) {
      console.error('gitstation: unexpected failure in a query', error);
    }
  }, [error]);
  const failure = failureOf(error);
  const message =
    failure.kind === 'api'
      ? failure.status === 403
        ? t('error.forbiddenTitle')
        : failure.userMessage
      : failure.kind === 'unauthenticated'
        ? t('login.expired')
        : failure.kind === 'network'
          ? t('error.network')
          : t('error.unexpected');
  const requestId = failure.kind === 'network' ? null : failure.requestId;
  // Asking again does not change "you may not" or an ended session (the
  // page's own error shows the sign-in).
  const retryable = failure.kind !== 'unauthenticated' && !(failure.kind === 'api' && failure.status === 403);
  return (
    <div className={hidden ? styles.collapsed : styles.inline} data-testid="inline-error">
      <p className={hidden ? styles.srOnly : styles.message}>{message}</p>
      {requestId !== null && (
        <p className={hidden ? styles.srOnly : styles.requestId}>
          {t('error.requestId')}
          {': '}
          <code data-testid="inline-request-id">{requestId}</code>
        </p>
      )}
      {retryable && (
        <button type="button" className={hidden ? styles.iconButton : styles.retry} onClick={onRetry}>
          <svg
            viewBox="0 0 24 24"
            width="16"
            height="16"
            aria-hidden="true"
            focusable="false"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <path d="M20 11a8 8 0 1 0-2.3 5.7" />
            <path d="M20 4v7h-7" />
          </svg>
          <span className={hidden ? styles.srOnly : undefined}>{t('error.retry')}</span>
        </button>
      )}
    </div>
  );
}
