// "My projects" (the ones I hold a role in, `?membership=true`) with the
// name/path filter: one component for Home and the projects page (plan 15
// §3.3), so the list, the filter and the three states live in one place.
import { useQuery } from '@tanstack/react-query';
import { useId, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { useApi } from '../api/context';
import { projectsQuery } from '../api/lists';
import controls from '../components/controls.module.css';
import { Loading } from '../components/Loading';
import { QueryError } from '../components/QueryError';
import { ProjectList } from './Lists';
import styles from './Lists.module.css';

/** `onCreate`: the empty state's next step (opens the create dialog). */
export function MyProjects({ onCreate }: { onCreate: () => void }) {
  const { t } = useTranslation();
  const api = useApi();
  const filterId = useId();
  const [filter, setFilter] = useState('');
  const mine = useQuery(projectsQuery(api, { membership: true }));

  if (mine.isPending) return <Loading />;
  if (mine.isError) {
    return (
      <QueryError
        error={mine.error}
        onRetry={() => {
          void mine.refetch();
        }}
      />
    );
  }
  if (mine.data.items.length === 0) {
    return (
      <div className={styles.empty}>
        <p>{t('home.mineEmpty')}</p>
        <p className={styles.hint}>{t('home.mineEmptyHint')}</p>
        <button type="button" className={controls.primary} onClick={onCreate}>
          {t('home.newProject')}
        </button>
      </div>
    );
  }

  const needle = filter.trim().toLowerCase();
  const shown = mine.data.items.filter(
    (p) =>
      needle === '' ||
      p.display_name.toLowerCase().includes(needle) ||
      p.path.toLowerCase().includes(needle),
  );
  return (
    <>
      <div className={controls.field}>
        <label htmlFor={filterId} className={controls.label}>
          {t('home.filter')}
        </label>
        <input
          id={filterId}
          type="search"
          className={controls.input}
          value={filter}
          onChange={(e) => {
            setFilter(e.target.value);
          }}
        />
      </div>
      {shown.length === 0 ? <p role="status">{t('home.filterEmpty')}</p> : <ProjectList items={shown} />}
      {mine.data.truncated && <p className={styles.hint}>{t('common.truncated')}</p>}
    </>
  );
}
