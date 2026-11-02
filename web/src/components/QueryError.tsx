// A failed query as the page shows it:
// - the session ended (401): the sign-in button, which returns to this page;
// - forbidden (403): its own title, so "you may not" is not read as "it broke";
// - anything else: ErrorNotice.
// An error that is not an ApiFailure is a bug in the page: logged once here.
import { useEffect, useId } from 'react';
import { useTranslation } from 'react-i18next';
import { useLocation } from 'react-router';

import { ApiFailure, failureOf } from '../api/client';
import { SignInPanel } from '../features/SignInPanel';
import { ErrorNotice } from './ErrorNotice';

export function QueryError({ error, onRetry }: { error: unknown; onRetry?: () => void }) {
  const { t } = useTranslation();
  const location = useLocation();
  const titleId = useId();
  useEffect(() => {
    if (!(error instanceof ApiFailure)) {
      console.error('gitstation: unexpected failure in a page query', error);
    }
  }, [error]);

  if (error instanceof ApiFailure && error.failure.kind === 'unauthenticated') {
    return (
      <section role="alert" aria-labelledby={titleId}>
        <h2 id={titleId}>{t('login.expiredTitle')}</h2>
        <p>{t('login.expired')}</p>
        <SignInPanel next={location.pathname + location.search} />
      </section>
    );
  }
  const failure = failureOf(error);
  if (failure.kind === 'api' && failure.status === 403) {
    return <ErrorNotice failure={failure} title={t('error.forbiddenTitle')} />;
  }
  return <ErrorNotice failure={failure} {...(onRetry ? { onRetry } : {})} />;
}
