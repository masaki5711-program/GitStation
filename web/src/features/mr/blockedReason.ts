// "Why it is stopped, and what to do next" in one line (plan 08), from the
// status contract (docs/api/merge-status.md, schema_version 1). Pure: the
// caller renders `t(key, values)`. Only the first reason is shown; anything
// not understood becomes the generic line, never an exception.
//
// `next` points at what the screen has: the threads, the changes, a push,
// and (U2) "Mark as ready" when the viewer flags allow it. The conditions
// panel (ConditionsPanel) words every reason with reasonLine().
import type { MergeStatus, StatusReason } from '../../api/types';

export type NextTarget = 'first_unresolved' | 'changes' | 'push' | 'mark_ready' | 'wait' | 'none';

export interface BlockedReason {
  key: string;
  values: Record<string, string | number>;
  next: NextTarget;
}

// The dictionary gate reads literal translate-call keys; this identity keeps the
// keys literal here while the caller does the translating.
const t = <K extends string>(key: K): K => key;

/** The word for each `next` that has something to press (shown beside the
 * line); `wait` / `none`: null, and the line's hint says what happens next. */
export function nextKey(next: NextTarget): string | null {
  switch (next) {
    case 'first_unresolved':
      return t('mr.status.next.firstUnresolved');
    case 'changes':
      return t('mr.status.next.changes');
    case 'push':
      return t('mr.status.next.push');
    case 'mark_ready':
      return t('mr.status.next.markReady');
    // Nothing to press: the line's hint (statusHint) names what is waited for.
    case 'wait':
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
  const line = reasonLine(first);
  // A draft the viewer may take out of draft: "next" is that button (the
  // flag is the server's answer, never guessed from a role).
  if (first.reason === 'draft' && status.viewer.can_mark_ready) return { ...line, next: 'mark_ready' };
  return line;
}

/** One reason in words (the status line's first, every line of the panel). */
export function reasonLine(first: StatusReason): BlockedReason {
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
    // M3 I1d: linking happens through the title, the branch name or gstn;
    // the hint (statusHint) says how.
    case 'work_item_link_missing':
      return plain(t('mr.status.workItemLinkMissing'), 'none');
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
    // M4 C7: the run is on an older merge result; the pipeline on the new
    // one decides (FG-CI-0013).
    case 'required_check_stale': {
      const name = text(d.name);
      if (name === null) return generic();
      return { key: t('mr.status.checkStale'), values: { name }, next: 'wait' };
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

/** For a line with nothing to press: what (or who) is waited for, or what
 * to do instead. Null when the line says it all or something can be pressed. */
export function statusHint(status: MergeStatus): string | null {
  if (status.mergeable) return null;
  switch (status.reasons[0]?.reason) {
    case 'approvals_missing':
      return t('mr.status.hint.approvals');
    case 'codeowner_approval_missing':
      return t('mr.status.hint.owners');
    case 'required_check_missing':
      return t('mr.status.hint.checks');
    case 'required_check_stale':
      return t('mr.status.hint.checksStale');
    case 'merge_in_progress':
      return t('mr.status.hint.merging');
    case 'config_change_needs_maintainer_approval':
      return t('mr.status.hint.maintainer');
    case 'changed_paths_unknown':
      return t('mr.status.hint.retry');
    case 'draft':
      return status.viewer.can_mark_ready ? null : t('mr.status.hint.draft');
    case 'not_open':
      return t('mr.status.hint.notOpen');
    case 'codeowners_invalid':
      return t('mr.status.hint.codeowners');
    case 'nothing_to_merge':
      return t('mr.status.hint.nothing');
    case 'work_item_link_missing':
      return t('mr.status.hint.workItem');
    default:
      return null;
  }
}

export type StatusTone = 'ready' | 'blocked' | 'info';

/** The colour of the line: `ready` (green), `blocked` (orange: someone has to
 * act), `info` (neutral: it is moving, or it is not a merge question). */
export function statusTone(status: MergeStatus): StatusTone {
  if (status.mergeable) return 'ready';
  switch (status.reasons[0]?.reason) {
    case 'mergeability_unknown':
    case 'merge_in_progress':
    case 'draft':
    case 'not_open':
    case 'nothing_to_merge':
      return 'info';
    default:
      return 'blocked';
  }
}

/** Why an approval does not count (`uncounted_reason`), as words. A name
 * this screen does not know is the generic "it does not count now". */
export function uncountedKey(reason: string | null): string {
  switch (reason) {
    case 'stale':
      return t('mr.uncounted.stale');
    case 'inactive':
      return t('mr.uncounted.inactive');
    case 'not_human':
      return t('mr.uncounted.notHuman');
    case 'role_insufficient':
      return t('mr.uncounted.roleInsufficient');
    case 'author':
      return t('mr.uncounted.author');
    case 'committer':
      return t('mr.uncounted.committer');
    case 'committers_unknown':
      return t('mr.uncounted.committersUnknown');
    case 'not_in_rule':
      return t('mr.uncounted.notInRule');
    case 'carried':
      return t('mr.uncounted.carried');
    default:
      return t('mr.uncounted.other');
  }
}
