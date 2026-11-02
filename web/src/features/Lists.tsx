// Project and group lists, as presented on Home and on the group pages.
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import type { ListedGroup, Project } from '../api/types';
import controls from '../components/controls.module.css';
import { roleLabel, visibilityLabel } from './labels';
import styles from './Lists.module.css';

export function ProjectList({ items }: { items: Project[] }) {
  const { t } = useTranslation();
  return (
    <ul className={styles.list}>
      {items.map((p) => {
        const role = roleLabel(t, p.role);
        return (
          <li key={p.id} className={styles.item}>
            <Link to={`/${p.path}`} className={styles.name}>
              {p.display_name}
            </Link>
            <code className={styles.path}>{p.path}</code>
            <span className={styles.badges}>
              <span className={controls.badge}>{visibilityLabel(t, p.visibility)}</span>
              {role !== null && <span className={controls.badge}>{role}</span>}
            </span>
          </li>
        );
      })}
    </ul>
  );
}

export function GroupList({ items }: { items: ListedGroup[] }) {
  const { t } = useTranslation();
  return (
    <ul className={styles.list}>
      {items.map((g) => {
        const role = roleLabel(t, g.role);
        return (
          <li key={g.id} className={styles.item}>
            <Link to={`/${g.path}`} className={styles.name}>
              {g.display_name}
            </Link>
            <code className={styles.path}>{g.path}</code>
            <span className={styles.badges}>
              {role !== null && <span className={controls.badge}>{role}</span>}
            </span>
          </li>
        );
      })}
    </ul>
  );
}
