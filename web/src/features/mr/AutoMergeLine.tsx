// Auto-merge on the merge request screen (M4 C9, ADR-0017 Amendment
// 2026-11-13; plan 08): one line saying what auto-merge does with this merge
// request — queued, waiting for what, or why it stopped — and what to press
// next. It stops only for program breakage, database destruction, a leak,
// code that runs on developers' PCs, a head whose risk cannot be shown, or an
// emergency stop; a warning never stops it. The error code is never shown
// (the request id of the decision is, for a support question).
import type { TFunction } from 'i18next';
import { useTranslation } from 'react-i18next';

import { useMergeRequestAutoMerge, type AutoMergeState, type MergeRequestAutoMerge } from '../../api/autoMerge';
import styles from './Mr.module.css';

/** The kind of line a state is shown as. */
export type LineKind =
  | 'instance_stopped'
  | 'project_stopped'
  | 'data_loss'
  | 'leak'
  | 'developer_pc'
  | 'breakage_check'
  | 'breakage_queue'
  | 'breakage_risk'
  | 'not_assessed'
  | 'draft'
  | 'risk_pending'
  | 'approvals'
  | 'protected_path'
  | 'conflict'
  | 'waiting'
  | 'held'
  | 'queued'
  | 'queued_warning'
  | 'queued_by_person';

/** The line for a merge request (null: auto-merge says nothing). */
export function lineKind(m: MergeRequestAutoMerge): LineKind | null {
  if (!m.acts) return null;
  if (m.stop === 'instance') return 'instance_stopped';
  if (m.stop === 'project') return 'project_stopped';
  const s = m.auto_merge;
  return s === null ? null : stateKind(s);
}

export function stateKind(s: AutoMergeState): LineKind {
  switch (s.state) {
    case 'stopped':
      switch (s.class) {
        case 'emergency_stop':
          return s.reason === 'instance_stopped' ? 'instance_stopped' : 'project_stopped';
        case 'data_loss':
          return 'data_loss';
        case 'leak':
          return 'leak';
        case 'developer_pc':
          return 'developer_pc';
        case 'program_breakage':
          if (s.reason === 'merge_queue_checks_failed') return 'breakage_queue';
          if (s.reason === 'risk_stop') return 'breakage_risk';
          return 'breakage_check';
        default:
          return 'not_assessed';
      }
    case 'held':
      return 'held';
    case 'queued':
      if (s.reason === 'queued_by_person') return 'queued_by_person';
      return s.reason === 'queued_with_warning' ? 'queued_warning' : 'queued';
    default:
      switch (s.reason) {
        case 'draft':
          return 'draft';
        case 'risk_pending':
          return 'risk_pending';
        case 'approvals_missing':
        case 'codeowner_approval_missing':
        case 'config_change_needs_maintainer_approval':
          return 'approvals';
        case 'protected_path':
          return 'protected_path';
        case 'conflict':
          return 'conflict';
        default:
          return 'waiting';
      }
  }
}

/** Why, in words (`detail`: the forge/risk rule or the check). */
export function whyText(t: TFunction, kind: LineKind, detail: string): string {
  switch (kind) {
    case 'instance_stopped':
      return t('autoMerge.why.instance_stopped');
    case 'project_stopped':
      return t('autoMerge.why.project_stopped');
    case 'data_loss':
      return t('autoMerge.why.data_loss', { detail });
    case 'leak':
      return t('autoMerge.why.leak', { detail });
    case 'developer_pc':
      return t('autoMerge.why.developer_pc', { detail });
    case 'breakage_check':
      return t('autoMerge.why.breakage_check', { detail });
    case 'breakage_queue':
      return t('autoMerge.why.breakage_queue');
    case 'breakage_risk':
      return t('autoMerge.why.breakage_risk', { detail });
    case 'not_assessed':
      return t('autoMerge.why.not_assessed');
    case 'draft':
      return t('autoMerge.why.draft');
    case 'risk_pending':
      return t('autoMerge.why.risk_pending');
    case 'approvals':
      return t('autoMerge.why.approvals');
    case 'protected_path':
      return t('autoMerge.why.protected_path');
    case 'conflict':
      return t('autoMerge.why.conflict');
    case 'waiting':
      return t('autoMerge.why.waiting');
    case 'held':
      return t('autoMerge.why.held');
    case 'queued':
      return t('autoMerge.why.queued');
    case 'queued_warning':
      return t('autoMerge.why.queued_warning');
    case 'queued_by_person':
      return t('autoMerge.why.queued_by_person');
  }
}

/** What to press next. */
export function nextText(t: TFunction, kind: LineKind): string {
  switch (kind) {
    case 'instance_stopped':
      return t('autoMerge.next.instance_stopped');
    case 'project_stopped':
      return t('autoMerge.next.project_stopped');
    case 'data_loss':
    case 'leak':
    case 'developer_pc':
    case 'breakage_risk':
    case 'not_assessed':
      return t('autoMerge.next.person_decides');
    case 'breakage_check':
    case 'breakage_queue':
      return t('autoMerge.next.fix_and_push');
    case 'draft':
      return t('autoMerge.next.draft');
    case 'risk_pending':
    case 'queued':
    case 'queued_warning':
    case 'queued_by_person':
      return t('autoMerge.next.nothing');
    case 'approvals':
      return t('autoMerge.next.approvals');
    case 'protected_path':
    case 'held':
      return t('autoMerge.next.merge_when_ready');
    case 'conflict':
      return t('autoMerge.next.conflict');
    case 'waiting':
      return t('autoMerge.next.waiting');
  }
}

/** Stopped lines are warnings; the rest are plain status. */
const STOPS: readonly LineKind[] = [
  'instance_stopped',
  'project_stopped',
  'data_loss',
  'leak',
  'developer_pc',
  'breakage_check',
  'breakage_queue',
  'breakage_risk',
  'not_assessed',
];

export function AutoMergeLine({ projectId, iid, open }: { projectId: number; iid: number; open: boolean }) {
  const { t } = useTranslation();
  const read = useMergeRequestAutoMerge(projectId, iid, open);
  // The line is advice: a failed read shows nothing (the merge controls
  // below still work and show their own errors).
  if (!open || read.data === undefined) return null;
  const kind = lineKind(read.data);
  if (kind === null) return null;
  const detail = read.data.auto_merge?.detail ?? '';
  const stopped = STOPS.includes(kind);
  return (
    <p
      role="status"
      className={stopped ? styles.autoMergeStopped : styles.nextHint}
      data-testid="mr-auto-merge"
      data-kind={kind}
    >
      <strong>{t('autoMerge.label')}</strong> {whyText(t, kind, detail)} {nextText(t, kind)}
    </p>
  );
}
