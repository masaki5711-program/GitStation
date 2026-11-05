// The one-line "why it is stopped and what to press next" (plan 08; the
// contract is docs/api/merge-status.md, schema_version 1).
import { describe, expect, it } from 'vitest';

import type { MergeStatus } from '../src/api/types';
import {
  blockedReason,
  nextKey,
  statusHint,
  statusTone,
  type NextTarget,
} from '../src/features/mr/blockedReason';

function status(
  reasons: { reason: string; detail: string | null }[],
  mergeable = false,
): MergeStatus {
  return {
    schema_version: 1,
    iid: 3,
    head_sha: '2'.repeat(40),
    target_branch: 'main',
    method: 'merge_commit',
    mergeable,
    // A newer server may add reason names under schema_version 1 (docs/api/merge-status.md);
    // tests feed unknown names on purpose, so the wire type is widened here only.
    reasons: reasons as MergeStatus['reasons'],
    approvals: null,
    checks: [],
    discussions: null,
    work_item_link: null,
    codeowners: { enforced: true, suspended_until: null },
    viewer: {
      can_approve: false,
      can_unapprove: false,
      can_merge: false,
      can_mark_ready: false,
      approved_by_me: false,
    },
  } satisfies MergeStatus;
}

const plain = (reason: string) => blockedReason(status([{ reason, detail: null }]));

describe('blockedReason: mergeable and unknown input', () => {
  it('says it can be merged when mergeable', () => {
    const r = blockedReason(status([], true));
    expect(r.key).toBe('mr.status.mergeable');
    expect(r.values).toEqual({});
  });

  it('mergeable wins over a stale reasons list', () => {
    expect(blockedReason(status([{ reason: 'draft', detail: null }], true)).key).toBe(
      'mr.status.mergeable',
    );
  });

  it('is generic when not mergeable and no reason is given', () => {
    expect(blockedReason(status([])).key).toBe('mr.status.generic');
  });

  it('is generic for an unknown reason name, without throwing', () => {
    const r = blockedReason(status([{ reason: 'from_the_future', detail: 'x=1' }]));
    expect(r.key).toBe('mr.status.generic');
    expect(r.next).toBe('changes');
  });

  it('uses only the first reason', () => {
    const r = blockedReason(
      status([
        { reason: 'draft', detail: null },
        { reason: 'conflict', detail: null },
      ]),
    );
    expect(r.key).toBe('mr.status.draft');
  });
});

describe('blockedReason: plain reasons', () => {
  const cases: [string, string, NextTarget][] = [
    ['not_open', 'mr.status.notOpen', 'none'],
    ['draft', 'mr.status.draft', 'none'],
    ['merge_in_progress', 'mr.status.mergeInProgress', 'wait'],
    ['source_branch_missing', 'mr.status.sourceBranchMissing', 'push'],
    ['mergeability_unknown', 'mr.status.mergeabilityUnknown', 'wait'],
    ['conflict', 'mr.status.conflict', 'push'],
    ['ff_not_possible', 'mr.status.ffNotPossible', 'push'],
    ['nothing_to_merge', 'mr.status.nothingToMerge', 'none'],
    ['config_change_needs_maintainer_approval', 'mr.status.configChange', 'wait'],
    // M3 I1d: nothing on this screen links a work item; the hint says how.
    ['work_item_link_missing', 'mr.status.workItemLinkMissing', 'none'],
  ];
  it.each(cases)('%s -> %s, next %s', (reason, key, next) => {
    expect(plain(reason)).toEqual({ key, values: {}, next });
  });
});

describe('blockedReason: reasons with a detail', () => {
  it('approvals_missing for a rule carries rule, required, counted', () => {
    const r = blockedReason(
      status([{ reason: 'approvals_missing', detail: 'rule=docs team required=2 counted=1' }]),
    );
    expect(r).toEqual({
      key: 'mr.status.approvalsMissing',
      values: { rule: 'docs team', required: 2, counted: 1 },
      next: 'wait',
    });
  });

  it('approvals_missing for @protected_branch hides the internal rule name', () => {
    const r = blockedReason(
      status([
        { reason: 'approvals_missing', detail: 'rule=@protected_branch required=1 counted=0' },
      ]),
    );
    expect(r.key).toBe('mr.status.approvalsMissingProtected');
    expect(r.values).toEqual({ required: 1, counted: 0 });
  });

  it('codeowner_approval_missing carries pattern and owners', () => {
    const r = blockedReason(
      status([{ reason: 'codeowner_approval_missing', detail: 'pattern=/docs/ owners=@a,@b' }]),
    );
    expect(r).toEqual({
      key: 'mr.status.codeownerMissing',
      values: { pattern: '/docs/', owners: '@a,@b' },
      next: 'wait',
    });
  });

  it('codeowners_invalid has a fixed line and no action for the author', () => {
    const r = blockedReason(
      status([{ reason: 'codeowners_invalid', detail: 'line=4 reason=bad' }]),
    );
    expect(r.key).toBe('mr.status.codeownersInvalid');
    expect(r.next).toBe('none');
  });

  it('required_check_missing carries the check name', () => {
    const r = blockedReason(
      status([{ reason: 'required_check_missing', detail: 'name=ci/gates' }]),
    );
    expect(r).toEqual({
      key: 'mr.status.checkMissing',
      values: { name: 'ci/gates' },
      next: 'wait',
    });
  });

  it('required_check_not_successful carries name, status, conclusion; next is push', () => {
    const r = blockedReason(
      status([
        {
          reason: 'required_check_not_successful',
          detail: 'name=ci/gates status=completed conclusion=failure',
        },
      ]),
    );
    expect(r).toEqual({
      key: 'mr.status.checkFailed',
      values: { name: 'ci/gates', status: 'completed', conclusion: 'failure' },
      next: 'push',
    });
  });

  it('unresolved_discussions carries the count and points at the first thread', () => {
    const r = blockedReason(status([{ reason: 'unresolved_discussions', detail: 'count=3' }]));
    expect(r).toEqual({
      key: 'mr.status.unresolved',
      values: { n: 3 },
      next: 'first_unresolved',
    });
  });

  it('changed_paths_unknown is a fixed line', () => {
    const r = blockedReason(
      status([{ reason: 'changed_paths_unknown', detail: 'reason=git_busy' }]),
    );
    expect(r.key).toBe('mr.status.changedPathsUnknown');
    expect(r.next).toBe('wait');
  });
});

describe('blockedReason: malformed detail never throws', () => {
  const bad: [string, string | null][] = [
    ['approvals_missing', null],
    ['approvals_missing', ''],
    ['approvals_missing', 'garbage'],
    ['approvals_missing', 'rule=x required=many counted=0'],
    ['approvals_missing', 'rule=x required=1'],
    ['codeowner_approval_missing', 'pattern='],
    ['required_check_missing', 'nonsense=1'],
    ['required_check_not_successful', 'name=x'],
    ['unresolved_discussions', 'count=-1'],
    ['unresolved_discussions', 'count=abc'],
    ['unresolved_discussions', null],
  ];
  it.each(bad)('%s with %j -> generic', (reason, detail) => {
    const r = blockedReason(status([{ reason, detail }]));
    expect(r.key).toBe('mr.status.generic');
    expect(r.values).toEqual({});
  });
});

describe('blockedReason: next never points at approve or merge (U2)', () => {
  it('every next is one of the U1 targets', () => {
    const allowed: NextTarget[] = ['first_unresolved', 'changes', 'push', 'wait', 'none'];
    const names = [
      'not_open',
      'draft',
      'merge_in_progress',
      'source_branch_missing',
      'mergeability_unknown',
      'conflict',
      'ff_not_possible',
      'nothing_to_merge',
      'approvals_missing',
      'codeowner_approval_missing',
      'codeowners_invalid',
      'required_check_missing',
      'required_check_not_successful',
      'unresolved_discussions',
      'config_change_needs_maintainer_approval',
      'changed_paths_unknown',
      'unknown_name',
    ];
    for (const n of names) {
      expect(allowed).toContain(plain(n).next);
    }
  });
});

describe('statusHint / statusTone (U1 review: name what is waited for)', () => {
  it.each([
    ['approvals_missing', 'mr.status.hint.approvals'],
    ['codeowner_approval_missing', 'mr.status.hint.owners'],
    ['required_check_missing', 'mr.status.hint.checks'],
    ['merge_in_progress', 'mr.status.hint.merging'],
    ['config_change_needs_maintainer_approval', 'mr.status.hint.maintainer'],
    ['changed_paths_unknown', 'mr.status.hint.retry'],
    ['draft', 'mr.status.hint.draft'],
    ['not_open', 'mr.status.hint.notOpen'],
    ['codeowners_invalid', 'mr.status.hint.codeowners'],
    ['nothing_to_merge', 'mr.status.hint.nothing'],
    ['work_item_link_missing', 'mr.status.hint.workItem'],
    ['mergeability_unknown', null],
    ['conflict', null],
    ['unresolved_discussions', null],
  ])('%s -> %s', (reason, hint) => {
    expect(statusHint(status([{ reason, detail: null }]))).toBe(hint);
  });

  it('a mergeable status has no hint', () => {
    expect(statusHint(status([], true))).toBeNull();
  });

  it('every wait and none reason has a hint (nothing to press is never silent)', () => {
    const names = [
      'not_open', 'draft', 'merge_in_progress', 'nothing_to_merge', 'codeowners_invalid',
      'config_change_needs_maintainer_approval', 'changed_paths_unknown', 'work_item_link_missing',
    ];
    for (const n of names) expect(statusHint(status([{ reason: n, detail: null }]))).not.toBeNull();
    expect(statusHint(status([{ reason: 'approvals_missing', detail: 'rule=r required=1 counted=0' }]))).not.toBeNull();
  });

  it('there is no bare "wait" word any more', () => {
    expect(nextKey('wait')).toBeNull();
  });

  it.each([
    [[], true, 'ready'],
    [[{ reason: 'mergeability_unknown', detail: null }], false, 'info'],
    [[{ reason: 'merge_in_progress', detail: null }], false, 'info'],
    [[{ reason: 'draft', detail: null }], false, 'info'],
    [[{ reason: 'not_open', detail: null }], false, 'info'],
    [[{ reason: 'nothing_to_merge', detail: null }], false, 'info'],
    [[{ reason: 'conflict', detail: null }], false, 'blocked'],
    [[{ reason: 'approvals_missing', detail: 'rule=r required=1 counted=0' }], false, 'blocked'],
  ] as const)('tone of %j is %s', (reasons, mergeable, tone) => {
    expect(statusTone(status([...reasons], mergeable))).toBe(tone);
  });
});
