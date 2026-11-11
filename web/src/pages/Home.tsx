// Home (plan 10 §4.2): my projects (the ones I hold a role in) with a filter
// (MyProjects, shared with /-/projects; plan 15 MR 7 reshapes Home),
// my groups and what I looked at recently (this browser only).
// No "coming soon" frames: a section appears when it has something to show,
// the main list always shows its loading / empty / error state.
import { useQuery } from '@tanstack/react-query';
import { useMemo, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { useApi } from '../api/context';
import { groupsQuery, projectsQuery } from '../api/lists';
import type { All } from '../api/pages';
import controls from '../components/controls.module.css';
import { QueryError } from '../components/QueryError';
import { CreateGroupDialog, CreateProjectDialog } from '../features/CreateDialogs';
import { GroupList } from '../features/Lists';
import { MyProjects } from '../features/MyProjects';
import { useDocumentTitle } from '../hooks/useDocumentTitle';
import { readRecent } from '../hooks/recent';
import { useSession } from '../session/session';
import styles from './Home.module.css';

/** `all`: the "see all" link to the full screen (plan 15 sidebar pages). */
function Block({
  id,
  title,
  all,
  children,
}: {
  id: string;
  title: string;
  all?: { to: string; label: string };
  children: ReactNode;
}) {
  return (
    <section aria-labelledby={id} className={styles.block}>
      <div className={styles.blockTop}>
        <h2 id={id} className={styles.heading}>
          {title}
        </h2>
        {all && (
          <Link to={all.to} className={styles.all}>
            {all.label}
          </Link>
        )}
      </div>
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
  const { user } = useSession();
  useDocumentTitle(t('home.title'));
  const [dialog, setDialog] = useState<'project' | 'group' | null>(null);
  const recent = useMemo(() => readRecent(), []);

  // The same query as MyProjects (one request, shared cache): when it fails,
  // its error is the one the page shows.
  const mine = useQuery(projectsQuery(api, { membership: true }));
  const groups = useQuery(groupsQuery(api, { membership: true }));

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
            {t('projects.new')}
          </button>
          {/* Only an administrator may create a top-level group (as /-/groups). */}
          {user.is_admin && (
            <button
              type="button"
              className={controls.button}
              onClick={() => {
                setDialog('group');
              }}
            >
              {t('groups.new')}
            </button>
          )}
        </div>
      </div>

      <Block id="home-mine" title={t('home.mine')} all={{ to: '/-/projects', label: t('home.allProjects') }}>
        <MyProjects
          onCreate={() => {
            setDialog('project');
          }}
        />
      </Block>

      {!mine.isError && groups.isError && (
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
        <Block id="home-groups" title={t('home.groups')} all={{ to: '/-/groups', label: t('home.allGroups') }}>
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
      {user.is_admin && (
        <CreateGroupDialog
          open={dialog === 'group'}
          onClose={() => {
            setDialog(null);
          }}
        />
      )}
    </>
  );
}
