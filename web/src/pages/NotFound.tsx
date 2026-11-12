// A path that does not exist or that the person may not see: the API answers
// both the same (no hint about what exists, ADR-0015), so does this page.
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { useDocumentTitle } from '../hooks/useDocumentTitle';
import { PageHeader } from '../ui/PageHeader';

export function NotFound() {
  const { t } = useTranslation();
  useDocumentTitle(t('notFound.title'));
  return (
    <section aria-labelledby="not-found-title">
      <PageHeader title={t('notFound.title')} titleId="not-found-title" />
      <p>{t('notFound.body')}</p>
      <p>
        <Link to="/">{t('notFound.home')}</Link>
      </p>
    </section>
  );
}
