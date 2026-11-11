// /-/projects (plan 15 §2.2 P3, §3.3, MR 5): two views as tabs — the
// projects I created (default, `?created_by=me`) and the ones I am a member
// of (`?tab=member`, `?membership=true`, inherited roles included). The tab
// is the `?tab=` query, so a link can point at one (as the account
// settings); each view is MyProjects with its filter and its three states.
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, useSearchParams } from 'react-router';

import controls from '../components/controls.module.css';
import { CreateProjectDialog } from '../features/CreateDialogs';
import { MyProjects, type ProjectScope } from '../features/MyProjects';
import { useDocumentTitle } from '../hooks/useDocumentTitle';
import styles from './Home.module.css';
import tabStyles from './settings/Settings.module.css';

const BASE = '/-/projects';

export function ProjectsPage() {
  const { t } = useTranslation();
  const [params] = useSearchParams();
  const tab: ProjectScope = params.get('tab') === 'member' ? 'member' : 'created';
  const tabs: { id: ProjectScope; label: string; to: string }[] = [
    { id: 'created', label: t('projects.tabCreated'), to: BASE },
    { id: 'member', label: t('projects.tabMember'), to: `${BASE}?tab=member` },
  ];
  useDocumentTitle(
    tab === 'member' ? `${t('projects.tabMember')} · ${t('projects.title')}` : t('projects.title'),
  );
  const [creating, setCreating] = useState(false);
  const create = () => {
    setCreating(true);
  };
  return (
    <>
      <div className={styles.top}>
        <h1 className={styles.title}>{t('projects.title')}</h1>
        <div className={styles.actions}>
          <button type="button" className={controls.primary} onClick={create}>
            {t('projects.new')}
          </button>
        </div>
      </div>
      <nav aria-label={t('projects.tabs')}>
        <ul className={tabStyles.tabs}>
          {tabs.map((x) => (
            <li key={x.id}>
              <Link
                to={x.to}
                className={tabStyles.tab}
                {...(tab === x.id ? { 'aria-current': 'page' as const } : {})}
              >
                {x.label}
              </Link>
            </li>
          ))}
        </ul>
      </nav>
      {tab === 'created' && <p className={tabStyles.hint}>{t('projects.createdHint')}</p>}
      <MyProjects key={tab} scope={tab} onCreate={create} />
      <CreateProjectDialog
        open={creating}
        onClose={() => {
          setCreating(false);
        }}
      />
    </>
  );
}
