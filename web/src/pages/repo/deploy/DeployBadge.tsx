// A deployment's status: a word, a glyph and a colour (never the colour alone).
import { useTranslation } from 'react-i18next';

import { deployStatusLabel, deployTone, type DeployTone } from '../../../features/deployApproval';
import styles from './Deploy.module.css';

const CLASS: Record<DeployTone, string | undefined> = {
  success: styles.badgeSuccess,
  failed: styles.badgeFailed,
  running: styles.badgeRunning,
  pending: styles.badgePending,
  neutral: styles.badgeNeutral,
};

export function DeployBadge({ status }: { status: string }) {
  const { t } = useTranslation();
  const tone = deployTone(status);
  return (
    <span className={`${styles.badge ?? ''} ${CLASS[tone] ?? ''}`} data-badge={tone}>
      {deployStatusLabel(t, status)}
    </span>
  );
}
