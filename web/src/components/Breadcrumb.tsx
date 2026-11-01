// The path as links to its ancestors (groups); the last segment is the
// current page.
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import styles from './Breadcrumb.module.css';

export function Breadcrumb({ path }: { path: string }) {
  const { t } = useTranslation();
  const segments = path.split('/');
  return (
    <nav aria-label={t('common.breadcrumb')} className={styles.nav}>
      <ol className={styles.list}>
        {segments.map((segment, i) => {
          const to = `/${segments.slice(0, i + 1).join('/')}`;
          const last = i === segments.length - 1;
          return (
            <li key={to} className={styles.item}>
              {last ? <span aria-current="page">{segment}</span> : <Link to={to}>{segment}</Link>}
            </li>
          );
        })}
      </ol>
    </nav>
  );
}
