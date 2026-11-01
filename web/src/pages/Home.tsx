// The placeholder home (M2 MR2): the first page of `GET /api/v1/projects`,
// or "not signed in" on a 401 (browser login arrives with MR4).
import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';

import { ApiFailure, call } from '../api/client';
import { useApi } from '../api/context';
import { ErrorNotice } from '../components/ErrorNotice';
import styles from './Home.module.css';

export function Home() {
  const { t } = useTranslation();
  const api = useApi();
  const projects = useQuery({
    queryKey: ['projects', 'first-page'],
    queryFn: ({ signal }) => call(() => api.GET('/api/v1/projects', { signal })),
  });

  return (
    <section aria-labelledby="home-title">
      <h1 id="home-title" className={styles.title}>
        {t('home.title')}
      </h1>
      {projects.isPending && <p role="status">{t('home.loading')}</p>}
      {projects.isError &&
        (projects.error instanceof ApiFailure &&
        projects.error.failure.kind === 'unauthenticated' ? (
          <div className={styles.signedOut} role="status">
            <p>{t('home.notLoggedIn')}</p>
            <p className={styles.hint}>{t('home.notLoggedInHint')}</p>
          </div>
        ) : (
          <ErrorNotice
            failure={
              projects.error instanceof ApiFailure ? projects.error.failure : { kind: 'network' }
            }
            onRetry={() => {
              void projects.refetch();
            }}
          />
        ))}
      {projects.isSuccess &&
        (projects.data.items.length === 0 ? (
          <p>{t('home.empty')}</p>
        ) : (
          <>
            <ul className={styles.list}>
              {projects.data.items.map((p) => (
                <li key={p.id} className={styles.item}>
                  <span className={styles.name}>{p.display_name}</span>
                  <code className={styles.path}>{p.path}</code>
                  <span className={styles.visibility}>
                    {p.visibility === 'internal'
                      ? t('home.visibility.internal')
                      : t('home.visibility.private')}
                  </span>
                </li>
              ))}
            </ul>
            {projects.data.has_more && <p className={styles.hint}>{t('home.more')}</p>}
          </>
        ))}
    </section>
  );
}
