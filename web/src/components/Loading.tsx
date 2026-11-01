// The loading state: a block with a reserved height, so the page does not
// jump when the content arrives; announced politely to assistive tech.
import { useTranslation } from 'react-i18next';

import styles from './Loading.module.css';

export function Loading() {
  const { t } = useTranslation();
  return (
    <div role="status" className={styles.block}>
      <span className={styles.text}>{t('common.loading')}</span>
    </div>
  );
}
