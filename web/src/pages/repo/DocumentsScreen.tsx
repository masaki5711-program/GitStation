// A project's documents in the plan 13 format (`/-/documents[?ref=&after=]`,
// plan 13 stage 3): one row per document, by id, with its title, type,
// classification (a word, not only a colour) and translations; paged by the
// server's `next_after`. An AI agent's list leaves confidential documents
// out — a person's never does — so `withheld` is shown only when it is there.
import { useTranslation } from 'react-i18next';
import { Link, useSearchParams } from 'react-router';

import { useDocuments } from '../../api/documents';
import { Loading } from '../../components/Loading';
import { documentUrl, documentsUrl } from '../../features/repoUrls';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { RepoError } from './common';
import { ClassificationBadge } from './DocumentScreen';
import styles from './Documents.module.css';
import { PageHeader } from '../../ui/PageHeader';

export function DocumentsScreen({ projectId, project }: { projectId: number; project: string }) {
  const { t } = useTranslation();
  const [params] = useSearchParams();
  const ref = params.get('ref') ?? undefined;
  const after = params.get('after') ?? undefined;
  const list = useDocuments(projectId, ref, after);
  useDocumentTitle(t('docs.title'));
  const keep = ref === undefined ? {} : { ref };

  let body;
  if (list.isPending) {
    body = <Loading />;
  } else if (list.isError) {
    body = (
      <RepoError
        error={list.error}
        onRetry={() => {
          void list.refetch();
        }}
      />
    );
  } else {
    const l = list.data;
    body = (
      <>
        <p className={styles.meta}>
          {t('docs.revision')}: <code>{l.ref.name}</code>
        </p>
        {l.incomplete !== null && (
          <p className={styles.warning} role="status" data-testid="docs-incomplete">
            {t('docs.incomplete', { code: l.incomplete.error_code })}
          </p>
        )}
        {l.withheld !== null && (
          <p className={styles.warning} role="status">
            {t('docs.withheld', { count: l.withheld.documents })}
          </p>
        )}
        {l.documents.length === 0 ? (
          <p data-testid="docs-empty">{t('docs.empty')}</p>
        ) : (
          <div className={styles.scroll} role="region" aria-label={t('docs.listLabel')} tabIndex={0}>
            <table className={styles.blocks}>
              <caption>{t('docs.listLabel')}</caption>
              <thead>
                <tr>
                  <th scope="col">{t('docs.id')}</th>
                  <th scope="col">{t('docs.docTitle')}</th>
                  <th scope="col">{t('docs.doctype')}</th>
                  <th scope="col">{t('docs.classification')}</th>
                  <th scope="col">{t('docs.translations')}</th>
                </tr>
              </thead>
              <tbody>
                {l.documents.map((d) => (
                  <tr key={d.id}>
                    <th scope="row">
                      <Link to={documentUrl(project, d.id, keep)}>{d.id}</Link>
                    </th>
                    <td>{d.title ?? t('docs.none')}</td>
                    <td>
                      <code>{d.doctype}</code>
                    </td>
                    <td>
                      <ClassificationBadge value={d.classification} />
                    </td>
                    <td>{d.translations.length === 0 ? t('docs.none') : d.translations.join(', ')}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        {(after !== undefined || l.next_after !== null) && (
          <nav aria-label={t('docs.pages')} className={styles.pages}>
            {after !== undefined && <Link to={documentsUrl(project, keep)}>{t('docs.firstPage')}</Link>}
            {l.next_after !== null && (
              <Link to={documentsUrl(project, { ...keep, after: l.next_after })}>{t('docs.nextPage')}</Link>
            )}
          </nav>
        )}
      </>
    );
  }
  return (
    <section aria-labelledby="docs-title">
      <PageHeader title={t('docs.title')} titleId="docs-title" />
      {body}
    </section>
  );
}
