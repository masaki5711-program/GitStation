// The loading state of a list: placeholder rows in the list's card (the page
// keeps its shape), announced politely as "loading" like Loading does (the
// bars themselves are hidden from assistive tech).
import { useTranslation } from 'react-i18next';

import styles from './Skeleton.module.css';

const ROWS = ['a', 'b', 'c'] as const;

export function SkeletonList() {
  const { t } = useTranslation();
  return (
    <div role="status" className={styles.list}>
      <span className={styles.srOnly}>{t('common.loading')}</span>
      {ROWS.map((k) => (
        <div key={k} className={styles.row} aria-hidden="true">
          <span className={styles.line} />
          <span className={styles.lineShort} />
        </div>
      ))}
    </div>
  );
}
