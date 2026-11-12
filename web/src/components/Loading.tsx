// The loading state: a block with a reserved height, so the page does not
// jump when the content arrives; announced politely to assistive tech. The
// words are in the status from its first render: a fast answer replaces the
// block before any delayed text would land, and the page must still say
// "loading" while it waits. No aria-busy here: a live region inside a busy
// one is held back until the wait ends.
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
