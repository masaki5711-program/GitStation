// The list of specifications (plan 10 §4.7): each one opens in the portal.
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import type { Project } from '../../api/types';
import { portalUrl } from '../../features/portal';
import styles from './Portal.module.css';

export function SpecList({ items }: { items: Project[] }) {
  const { t } = useTranslation();
  if (items.length === 0) return <p>{t('portal.listEmpty')}</p>;
  return (
    <ul className={styles.specList}>
      {items.map((p) => (
        <li key={p.id} className={styles.specItem}>
          <Link to={portalUrl(p.path)} className={styles.specName}>
            {p.display_name}
          </Link>
          {p.description !== '' && <span className={styles.specDescription}>{p.description}</span>}
        </li>
      ))}
    </ul>
  );
}
