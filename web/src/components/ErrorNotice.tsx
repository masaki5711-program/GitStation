// The one error display (CLAUDE.md contract 3): the server's
// `user_message` and the `request_id` — never the error code, a status,
// a stack or a raw body. The request id is shown untranslated and can be
// copied (ADR-0022 §6).
import { useState } from 'react';
import { useTranslation } from 'react-i18next';

import type { Failure } from '../api/client';
import styles from './ErrorNotice.module.css';

export interface ErrorNoticeProps {
  failure: Failure;
  /** Replaces the generic heading (a known state such as "sign-in is not configured"). */
  title?: string;
  onRetry?: () => void;
}

function requestIdOf(failure: Failure): string | null {
  switch (failure.kind) {
    case 'api':
      return failure.requestId;
    case 'unauthenticated':
    case 'unexpected':
      return failure.requestId;
    case 'network':
      return null;
  }
}

export function ErrorNotice({ failure, title, onRetry }: ErrorNoticeProps) {
  const { t } = useTranslation();
  const [copy, setCopy] = useState<'idle' | 'copied' | 'failed'>('idle');
  const message =
    failure.kind === 'api'
      ? failure.userMessage
      : failure.kind === 'network'
        ? t('error.network')
        : t('error.unexpected');
  const requestId = requestIdOf(failure);

  // The clipboard can be missing (plain http is not a secure context) or
  // refuse (permissions): say so; the id stays on screen, selectable.
  const copyId = (id: string) => {
    const clipboard = (navigator as Partial<Navigator>).clipboard;
    if (clipboard === undefined) {
      setCopy('failed');
      return;
    }
    clipboard.writeText(id).then(
      () => {
        setCopy('copied');
      },
      () => {
        setCopy('failed');
      },
    );
  };
  const copyLabel =
    copy === 'copied' ? t('error.copied') : copy === 'failed' ? t('error.copyFailed') : t('error.copy');

  return (
    <section className={styles.notice} role="alert" aria-labelledby="error-notice-title">
      <h2 id="error-notice-title" className={styles.title}>
        <span aria-hidden="true" className={styles.icon} />
        {title ?? t('error.title')}
      </h2>
      <p className={styles.message}>{message}</p>
      {requestId !== null && (
        <p className={styles.requestId}>
          <span>{t('error.requestId')}</span>
          {': '}
          <code data-testid="request-id">{requestId}</code>{' '}
          <button
            type="button"
            className={styles.button}
            onClick={() => {
              copyId(requestId);
            }}
          >
            {copyLabel}
          </button>
          <span className={styles.srOnly} aria-live="polite">
            {copy === 'idle' ? null : copyLabel}
          </span>
        </p>
      )}
      {onRetry && (
        <button type="button" className={styles.button} onClick={onRetry}>
          {t('error.retry')}
        </button>
      )}
    </section>
  );
}
