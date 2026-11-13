// M4 C8: when the merge request screen offers "merge when ready".
import { describe, expect, it } from 'vitest';

import { queueProves } from '../src/features/mr/QueueControl';

describe('queueProves', () => {
  it('accepts only the merge result ci/pipeline the queue settles itself', () => {
    expect(queueProves('required_check_missing', 'name=ci/pipeline')).toBe(true);
    expect(queueProves('required_check_stale', 'name=ci/pipeline')).toBe(true);
    expect(queueProves('required_check_not_successful', 'name=ci/pipeline')).toBe(true);
    expect(queueProves('required_check_missing', 'name=ext/lint')).toBe(false);
    expect(queueProves('conflict', null)).toBe(false);
    expect(queueProves('approvals_missing', 'rule=default required=1 counted=0')).toBe(false);
  });
});
