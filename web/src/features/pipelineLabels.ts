// Words for the CI pipeline API enums. Literal keys only (the dictionary
// gate reads them). An unknown value is shown as it is.
import type { TFunction } from 'i18next';

import type { BadgeTone } from './mr/StateBadge';

export function pipelineStatusLabel(t: TFunction, status: string): string {
  switch (status) {
    case 'pending':
      return t('pipelines.status.pending');
    case 'running':
      return t('pipelines.status.running');
    case 'succeeded':
      return t('pipelines.status.succeeded');
    case 'failed':
      return t('pipelines.status.failed');
    case 'canceled':
      return t('pipelines.status.canceled');
    case 'skipped':
      return t('pipelines.status.skipped');
    case 'invalid':
      return t('pipelines.status.invalid');
    case 'error':
      return t('pipelines.status.error');
    case 'created':
      return t('pipelines.status.created');
    default:
      return status;
  }
}

/** A pipeline's or job's status as the shared badge: the word is always
 * there, the colour only repeats it. */
export function pipelineTone(status: string): BadgeTone {
  switch (status) {
    case 'succeeded':
      return 'open';
    case 'failed':
    case 'invalid':
    case 'error':
      return 'unresolved';
    case 'canceled':
    case 'skipped':
      return 'closed';
    default:
      return 'draft';
  }
}

export function pipelineSourceLabel(t: TFunction, source: string): string {
  switch (source) {
    case 'push':
      return t('pipelines.source.push');
    case 'merge_request':
      return t('pipelines.source.mergeRequest');
    case 'schedule':
      return t('pipelines.source.schedule');
    case 'manual':
      return t('pipelines.source.manual');
    default:
      return source;
  }
}

/** Who started it: the API gives a user id only (no name). */
export function triggeredByLabel(t: TFunction, by: number | null, me: number): string {
  if (by === null) return t('pipelines.by.system');
  if (by === me) return t('pipelines.by.you');
  return t('pipelines.by.user', { id: by });
}

/** `from` → `to` as "1分 5秒"; null when either end is missing or unreadable. */
export function formatDuration(t: TFunction, from: string | null, to: string | null): string | null {
  if (from === null || to === null) return null;
  const ms = new Date(to).getTime() - new Date(from).getTime();
  if (Number.isNaN(ms) || ms < 0) return null;
  const total = Math.round(ms / 1000);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  if (h > 0) return t('pipelines.duration.hours', { h, m });
  if (m > 0) return t('pipelines.duration.minutes', { m, s });
  return t('pipelines.duration.seconds', { s });
}
