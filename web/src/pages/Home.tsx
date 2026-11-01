// Home (plan 10 §4.2): my projects (the ones I hold a role in) with a filter,
// specifications, my groups and what I looked at recently (this browser only).
// No "coming soon" frames: a section appears when it has something to show,
// the main list always shows its loading / empty / error state.
import { useQuery } from '@tanstack/react-query';
import { useMemo, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { useApi } from '../api/context';
import { Loading } from '../components/Loading';
import { groupsQuery, projectsQuery } from '../api/lists';
import type { All } from '../api/pages';
import controls from '../components/controls.module.css';
import { QueryError } from '../components/QueryError';
import { CreateGroupDialog, CreateProjectDialog } from '../features/CreateDialogs';
import { GroupList, ProjectList } from '../features/Lists';
import { useDocumentTitle } from '../hooks/useDocumentTitle';
import { readRecent } from '../hooks/recent';
import styles from './Home.module.css';

function Block({ id, title, children }: { id: string; title: string; children: ReactNode }) {
  return (
    <section aria-labelledby={id} className={styles.block}>
      <h2 id={id} className={styles.heading}>
        {title}
      </h2>
      {children}
    </section>
  );
}

function Truncated({ all }: { all: All<unknown> }) {
  const { t } = useTranslation();
  return all.truncated ? <p className={styles.hint}>{t('common.truncated')}</p> : null;
}

export function Home() {
  const { t } = useTranslation();
  const api = useApi();
  useDocumentTitle(t('home.title'));
  const [dialog, setDialog] = useState<'project' | 'group' | null>(null);
  const [filter, setFilter] = useState('');
  const recent = useMemo(() => readRecent(), []);

  const mine = useQuery(projectsQuery(api, { membership: true }));
  const specs = useQuery(projectsQuery(api, { kind: 'spec' }));
  const groups = useQuery(groupsQuery(api, { membership: true }));

  const needle = filter.trim().toLowerCase();
  const shown =
    mine.data?.items.filter(
      (p) =>
        needle === '' ||
        p.display_name.toLowerCase().includes(needle) ||
        p.path.toLowerCase().includes(needle),
    ) ?? [];

  return (
    <>
      <div className={styles.top}>
        <h1 className={styles.title}>{t('home.title')}</h1>
        <div className={styles.actions}>
          <button
            type="button"
            className={controls.primary}
            onClick={() => {
              setDialog('project');
            }}
          >
            {t('home.newProject')}
          </button>
          <button
            type="button"
            className={controls.button}
            onClick={() => {
              setDialog('group');
            }}
          >
            {t('home.newGroup')}
          </button>
        </div>
      </div>

      <Block id="home-mine" title={t('home.mine')}>
        {mine.isPending && <Loading />}
        {mine.isError && (
          <QueryError
            error={mine.error}
            onRetry={() => {
              void mine.refetch();
            }}
          />
        )}
        {mine.isSuccess &&
          (mine.data.items.length === 0 ? (
            <div className={styles.empty}>
              <p>{t('home.mineEmpty')}</p>
              <p className={styles.hint}>{t('home.mineEmptyHint')}</p>
              <button
                type="button"
                className={controls.primary}
                onClick={() => {
                  setDialog('project');
                }}
              >
                {t('home.newProject')}
              </button>
            </div>
          ) : (
            <>
              <div className={controls.field}>
                <label htmlFor="home-filter" className={controls.label}>
                  {t('home.filter')}
                </label>
                <input
                  id="home-filter"
                  type="search"
                  className={controls.input}
                  value={filter}
                  onChange={(e) => {
                    setFilter(e.target.value);
                  }}
                />
              </div>
              {shown.length === 0 ? (
                <p role="status">{t('home.filterEmpty')}</p>
              ) : (
                <ProjectList items={shown} />
              )}
              <Truncated all={mine.data} />
            </>
          ))}
      </Block>

      {!mine.isError && specs.isError && (
        <Block id="home-specs" title={t('home.specs')}>
          <QueryError
            error={specs.error}
            onRetry={() => {
              void specs.refetch();
            }}
          />
        </Block>
      )}
      {specs.isSuccess && specs.data.items.length > 0 && (
        <Block id="home-specs" title={t('home.specs')}>
          <ProjectList items={specs.data.items} />
          <Truncated all={specs.data} />
        </Block>
      )}

      {!mine.isError && !specs.isError && groups.isError && (
        <Block id="home-groups" title={t('home.groups')}>
          <QueryError
            error={groups.error}
            onRetry={() => {
              void groups.refetch();
            }}
          />
        </Block>
      )}
      {groups.isSuccess && groups.data.items.length > 0 && (
        <Block id="home-groups" title={t('home.groups')}>
          <GroupList items={groups.data.items} />
          <Truncated all={groups.data} />
        </Block>
      )}

      {recent.length > 0 && (
        <Block id="home-recent" title={t('home.recent')}>
          <ul className={styles.recent}>
            {recent.map((r) => (
              <li key={r.path}>
                <Link to={`/${r.path}`}>{r.name}</Link> <code className={styles.path}>{r.path}</code>
              </li>
            ))}
          </ul>
          <p className={styles.hint}>{t('home.recentHint')}</p>
        </Block>
      )}

      <CreateProjectDialog
        open={dialog === 'project'}
        onClose={() => {
          setDialog(null);
        }}
      />
      <CreateGroupDialog
        open={dialog === 'group'}
        onClose={() => {
          setDialog(null);
        }}
      />
    </>
  );
}
