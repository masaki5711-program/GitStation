// "Why it is stopped, and what to do next" in one line (plan 08), from the
// status contract (docs/api/merge-status.md, schema_version 1). Pure: the
// caller renders `t(key, values)`. Only the first reason is shown; anything
// not understood becomes the generic line, never an exception.
//
// `next` may only point at what U1 has. Approve and merge are U2.
import type { MergeStatus } from '../../api/types';

export type NextTarget = 'first_unresolved' | 'changes' | 'push' | 'wait' | 'none';

export interface BlockedReason {
  key: string;
  values: Record<string, string | number>;
  next: NextTarget;
}

// The dictionary gate reads literal translate-call keys; this identity keeps the
// keys literal here while the caller does the translating.
const t = <K extends string>(key: K): K => key;

/** The word for each `next` (shown beside the line); `none`: nothing to press. */
export function nextKey(next: NextTarget): string | null {
  switch (next) {
    case 'first_unresolved':
      return t('mr.status.next.firstUnresolved');
    case 'changes':
      return t('mr.status.next.changes');
    case 'push':
      return t('mr.status.next.push');
    case 'wait':
      return t('mr.status.next.wait');
    case 'none':
      return null;
  }
}

const generic = (): BlockedReason => ({ key: t('mr.status.generic'), values: {}, next: 'changes' });

/** `rule=a b required=1 counted=0` into pairs; a value runs to the next
 * ` key=` so a name with spaces survives. A string without pairs gives {}.
 *
 * Known limit: a value that itself contains ` word=` (a space, then lower
 * case letters or `_`, then `=`) is cut there and the rest read as another
 * pair. The server's names (rules, checks, CODEOWNERS patterns) do not take
 * that shape today; if one does, the line shows a wrong value or falls back
 * to the generic text, never throws. */
function parseDetail(detail: string | null): Record<string, string> {
  const out: Record<string, string> = {};
  if (detail === null) return out;
  const marks = [...detail.matchAll(/(?:^|\s)([a-z_]+)=/g)];
  marks.forEach((m, i) => {
    const start = m.index + m[0].length;
    const end = marks[i + 1]?.index ?? detail.length;
    const key = m[1];
    if (key !== undefined) out[key] = detail.slice(start, end).trim();
  });
  return out;
}

const count = (v: string | undefined): number | null =>
  v !== undefined && /^\d+$/.test(v) ? Number(v) : null;

const text = (v: string | undefined): string | null => (v === undefined || v === '' ? null : v);

export function blockedReason(status: MergeStatus): BlockedReason {
  if (status.mergeable) return { key: t('mr.status.mergeable'), values: {}, next: 'none' };
  const first = status.reasons[0];
  if (first === undefined) return generic();
  const d = parseDetail(first.detail);
  const plain = (key: string, next: NextTarget): BlockedReason => ({ key, values: {}, next });

  switch (first.reason) {
    case 'not_open':
      return plain(t('mr.status.notOpen'), 'none');
    case 'draft':
      return plain(t('mr.status.draft'), 'none');
    case 'merge_in_progress':
      return plain(t('mr.status.mergeInProgress'), 'wait');
    case 'source_branch_missing':
      return plain(t('mr.status.sourceBranchMissing'), 'push');
    case 'mergeability_unknown':
      return plain(t('mr.status.mergeabilityUnknown'), 'wait');
    case 'conflict':
      return plain(t('mr.status.conflict'), 'push');
    case 'ff_not_possible':
      return plain(t('mr.status.ffNotPossible'), 'push');
    case 'nothing_to_merge':
      return plain(t('mr.status.nothingToMerge'), 'none');
    case 'config_change_needs_maintainer_approval':
      return plain(t('mr.status.configChange'), 'wait');
    case 'changed_paths_unknown':
      return plain(t('mr.status.changedPathsUnknown'), 'wait');
    case 'codeowners_invalid':
      return plain(t('mr.status.codeownersInvalid'), 'none');
    case 'approvals_missing': {
      const rule = text(d.rule);
      const required = count(d.required);
      const counted = count(d.counted);
      if (rule === null || required === null || counted === null) return generic();
      if (rule === '@protected_branch') {
        return {
          key: t('mr.status.approvalsMissingProtected'),
          values: { required, counted },
          next: 'wait',
        };
      }
      return {
        key: t('mr.status.approvalsMissing'),
        values: { rule, required, counted },
        next: 'wait',
      };
    }
    case 'codeowner_approval_missing': {
      const pattern = text(d.pattern);
      const owners = text(d.owners);
      if (pattern === null || owners === null) return generic();
      return { key: t('mr.status.codeownerMissing'), values: { pattern, owners }, next: 'wait' };
    }
    case 'required_check_missing': {
      const name = text(d.name);
      if (name === null) return generic();
      return { key: t('mr.status.checkMissing'), values: { name }, next: 'wait' };
    }
    case 'required_check_not_successful': {
      const name = text(d.name);
      const checkStatus = text(d.status);
      const conclusion = text(d.conclusion);
      if (name === null || checkStatus === null || conclusion === null) return generic();
      return {
        key: t('mr.status.checkFailed'),
        values: { name, status: checkStatus, conclusion },
        next: 'push',
      };
    }
    case 'unresolved_discussions': {
      const n = count(d.count);
      if (n === null) return generic();
      return { key: t('mr.status.unresolved'), values: { n }, next: 'first_unresolved' };
    }
    default:
      return generic();
  }
}
