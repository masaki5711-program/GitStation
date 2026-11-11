// A pipeline's or job's status: a word, a glyph and a colour (never the
// colour alone). Running and pending look different: running is moving now,
// pending waits for a runner.
import { useTranslation } from 'react-i18next';

import { pipelineStatusLabel } from '../../../features/pipelineLabels';
import styles from './Pipelines.module.css';

export type PipelineTone = 'success' | 'failed' | 'running' | 'pending' | 'neutral';

export function pipelineTone(status: string): PipelineTone {
  switch (status) {
    case 'succeeded':
      return 'success';
    case 'failed':
    case 'invalid':
    case 'error':
      return 'failed';
    case 'running':
      return 'running';
    case 'canceled':
    case 'skipped':
      return 'neutral';
    default:
      return 'pending';
  }
}

const CLASS: Record<PipelineTone, string | undefined> = {
  success: styles.badgeSuccess,
  failed: styles.badgeFailed,
  running: styles.badgeRunning,
  pending: styles.badgePending,
  neutral: styles.badgeNeutral,
};

export function PipelineBadge({ status }: { status: string }) {
  const { t } = useTranslation();
  const tone = pipelineTone(status);
  return (
    <span className={`${styles.badge ?? ''} ${CLASS[tone] ?? ''}`} data-badge={tone}>
      {pipelineStatusLabel(t, status)}
    </span>
  );
}
