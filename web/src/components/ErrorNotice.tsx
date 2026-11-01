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

export function ErrorNotice({ failure, onRetry }: ErrorNoticeProps) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);
  const message =
    failure.kind === 'api'
      ? failure.userMessage
      : failure.kind === 'network'
        ? t('error.network')
        : t('error.unexpected');
  const requestId = requestIdOf(failure);

  const copy = (id: string) => {
    navigator.clipboard.writeText(id).then(
      () => {
        setCopied(true);
      },
      () => {
        // Clipboard refused (permissions): the id stays selectable on screen.
        setCopied(false);
      },
    );
  };

  return (
    <section className={styles.notice} role="alert" aria-labelledby="error-notice-title">
      <h2 id="error-notice-title" className={styles.title}>
        <span aria-hidden="true" className={styles.icon} />
        {t('error.title')}
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
              copy(requestId);
            }}
          >
            {copied ? t('error.copied') : t('error.copy')}
          </button>
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
