// Shown on the page a moved path led to (the URL rescue passes `movedFrom`
// in the navigation state): the old address keeps working.
import { useTranslation } from 'react-i18next';
import { useLocation } from 'react-router';

import styles from './MovedNotice.module.css';

function movedFrom(state: unknown): string | null {
  return typeof state === 'object' &&
    state !== null &&
    'movedFrom' in state &&
    typeof state.movedFrom === 'string'
    ? state.movedFrom
    : null;
}

export function MovedNotice() {
  const { t } = useTranslation();
  const from = movedFrom(useLocation().state);
  if (from === null) return null;
  return (
    <p role="status" className={styles.moved}>
      {t('moved.notice')} <code>{from}</code>
    </p>
  );
}
