// Pieces the two webhook screens share: the one-line reason a webhook is
// stopped (why it stopped, and what to press next) and the state chip.
import { useTranslation } from 'react-i18next';

import type { Webhook } from '../../api/webhooks';
import styles from '../settings/Settings.module.css';

/** Why a stopped webhook is stopped, and what to press next: one line. A
 * running webhook has none. `manual` is also the line for a reason this
 * screen does not know (a newer server): pressing Enable is still right. */
export function StopReason({ hook }: { hook: Pick<Webhook, 'enabled' | 'disabled_reason' | 'consecutive_failures'> }) {
  const { t } = useTranslation();
  if (hook.enabled) return null;
  let line: string;
  switch (hook.disabled_reason) {
    case 'failures':
      line = t('webhooks.stopped.failures', { count: hook.consecutive_failures });
      break;
    case 'failing_24h':
      line = t('webhooks.stopped.failing24h');
      break;
    default:
      line = t('webhooks.stopped.manual');
  }
  return <p className={styles.hint}>{line}</p>;
}

export function EnabledChip({ enabled }: { enabled: boolean }) {
  const { t } = useTranslation();
  return (
    <span className={`${styles.chip} ${enabled ? styles.chipActive : styles.chipExpired}`}>
      {enabled ? t('webhooks.enabled') : t('webhooks.disabled')}
    </span>
  );
}

/** `/-/webhooks/{id}/deliveries`: a positive integer id, or null. */
export function parseHookId(raw: string): number | null {
  return /^[1-9][0-9]{0,15}$/.test(raw) ? Number(raw) : null;
}
