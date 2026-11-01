// A failed query as the page shows it: the session ended (401) -> the
// sign-in button, which returns to this page; anything else -> ErrorNotice.
// An error that is not an ApiFailure is a bug in the page: logged once here.
import { useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { useLocation } from 'react-router';

import { ApiFailure, failureOf } from '../api/client';
import { SignInPanel } from '../features/SignInPanel';
import { ErrorNotice } from './ErrorNotice';

export function QueryError({ error, onRetry }: { error: unknown; onRetry?: () => void }) {
  const { t } = useTranslation();
  const location = useLocation();
  useEffect(() => {
    if (!(error instanceof ApiFailure)) {
      console.error('gitstation: unexpected failure in a page query', error);
    }
  }, [error]);

  if (error instanceof ApiFailure && error.failure.kind === 'unauthenticated') {
    return (
      <section role="alert" aria-labelledby="expired-title">
        <h2 id="expired-title">{t('login.expiredTitle')}</h2>
        <p>{t('login.expired')}</p>
        <SignInPanel next={location.pathname + location.search} />
      </section>
    );
  }
  return <ErrorNotice failure={failureOf(error)} {...(onRetry ? { onRetry } : {})} />;
}
