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
  useDocumentTitle(t('projectsPage.title'));
  const [creating, setCreating] = useState(false);
  const create = () => {
    setCreating(true);
  };
  return (
    <>
      <div className={styles.top}>
        <h1 className={styles.title}>{t('projectsPage.title')}</h1>
        <div className={styles.actions}>
          <button type="button" className={controls.primary} onClick={create}>
            {t('home.newProject')}
          </button>
        </div>
      </div>
      <section aria-labelledby="projects-member" className={styles.block}>
        <h2 id="projects-member" className={styles.heading}>
          {t('projectsPage.member')}
        </h2>
        <MyProjects onCreate={create} />
      </section>
      <CreateProjectDialog
        open={creating}
        onClose={() => {
          setCreating(false);
        }}
      />
    </>
  );
}
