// A path that does not exist or that the person may not see: the API answers
// both the same (no hint about what exists, ADR-0015), so does this page.
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { useDocumentTitle } from '../hooks/useDocumentTitle';

export function NotFound() {
  const { t } = useTranslation();
  useDocumentTitle(t('notFound.title'));
  return (
    <section aria-labelledby="not-found-title">
      <h1 id="not-found-title">{t('notFound.title')}</h1>
      <p>{t('notFound.body')}</p>
      <p>
        <Link to="/">{t('notFound.home')}</Link>
      </p>
    </section>
  );
}

/** A screen of a project that exists but is not built yet (repository
 * browsing arrives with the next release): say so, and offer the way back. */
export function Unavailable({ backTo }: { backTo: string }) {
  const { t } = useTranslation();
  useDocumentTitle(t('unavailable.title'));
  return (
    <section aria-labelledby="unavailable-title">
      <h1 id="unavailable-title">{t('unavailable.title')}</h1>
      <p>{t('unavailable.body')}</p>
      <p>
        <Link to={backTo}>{t('unavailable.back')}</Link>
      </p>
    </section>
  );
}
