// The AI asset releases screen's words that are decisions, not drawing
// (plan 17 P17-7b): the stage's name, and the one line per release saying
// why it stands where it does and what comes next (plan 08). The server
// decides every request again; these only read what it answered.
import type { TFunction } from 'i18next';

import type { Release } from '../api/aiReleases';

export function stageLabel(t: TFunction, environment: string): string {
  switch (environment) {
    case 'pfs-ai-shadow':
      return t('aiRelease.stage.shadow');
    case 'pfs-ai-canary':
      return t('aiRelease.stage.canary');
    case 'pfs-ai-prod':
      return t('aiRelease.stage.prod');
    default:
      return environment;
  }
}

/** How the one line is drawn: blocked (withdrawn, a revoked key), waiting
 * for an approval or under way, a next stage to request, or done. */
export type ReleaseTone = 'blocked' | 'waiting' | 'next' | 'done';

export function releaseTone(r: Release): ReleaseTone {
  if (r.withdrawal != null || r.key_revoked === true) return 'blocked';
  const stages = r.stages ?? [];
  if (stages.some((s) => s.status === 'pending_approval' || s.status === 'queued' || s.status === 'running')) {
    return 'waiting';
  }
  return r.next_stage != null ? 'next' : 'done';
}

/** The one line: withdrawn, a revoked key, waiting for an approval (and
 * where), under way, the next stage, or in production. */
export function releaseLine(t: TFunction, r: Release): string {
  if (r.withdrawal != null) return t('aiRelease.line.withdrawn');
  if (r.key_revoked === true) return t('aiRelease.line.keyRevoked');
  const stages = r.stages ?? [];
  const waiting = stages.find((s) => s.status === 'pending_approval');
  if (waiting !== undefined) {
    return t('aiRelease.line.waiting', { stage: stageLabel(t, waiting.environment), id: waiting.deployment_id });
  }
  const moving = stages.find((s) => s.status === 'queued' || s.status === 'running');
  if (moving !== undefined) return t('aiRelease.line.moving', { stage: stageLabel(t, moving.environment) });
  if (r.next_stage != null) {
    const failed = stages.find(
      (s) => s.environment === r.next_stage && s.status != null && s.status !== 'succeeded',
    );
    return failed === undefined
      ? t('aiRelease.line.next', { stage: stageLabel(t, r.next_stage) })
      : t('aiRelease.line.retry', { stage: stageLabel(t, r.next_stage) });
  }
  return t('aiRelease.line.done');
}
