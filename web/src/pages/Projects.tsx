// /-/projects (plan 15 §3.3, MR 1): the projects I hold a role in, with the
// filter (MyProjects, shared with Home). The "created by me" view comes with
// plan 15 MR 5; until then there is one view and no tab.
import { useState } from 'react';
import { useTranslation } from 'react-i18next';

import controls from '../components/controls.module.css';
import { CreateProjectDialog } from '../features/CreateDialogs';
import { MyProjects } from '../features/MyProjects';
import { useDocumentTitle } from '../hooks/useDocumentTitle';
import styles from './Home.module.css';

export function ProjectsPage() {
  const { t } = useTranslation();
  useDocumentTitle(t('projects.title'));
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
      <MyProjects onCreate={create} />
      <CreateProjectDialog
        open={creating}
        onClose={() => {
          setCreating(false);
        }}
      />
    </>
  );
}
