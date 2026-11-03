// The one-line "why it is stopped and what to press next" (plan 08; the
// contract is docs/api/merge-status.md, schema_version 1).
import { describe, expect, it } from 'vitest';

import type { MergeStatus } from '../src/api/types';
import { blockedReason, type NextTarget } from '../src/features/mr/blockedReason';

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
    reasons,
    approvals: null,
    checks: [],
    discussions: null,
    codeowners: { enforced: true, suspended_until: null },
    updated_at: null,
  } as MergeStatus;
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
