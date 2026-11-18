// M4 C8: when the merge request screen offers "merge when ready"; plan 18
// S1: the line of an entry the queue removed — why, and what to do next —
// in every language.
import i18next from 'i18next';
import { describe, expect, it } from 'vitest';

import type { QueueEntry } from '../src/api/mergeQueue';
import { entryLine, queueProves, reasonLabel, reasonNext } from '../src/features/mr/QueueControl';
import en from '../src/i18n/en.json';
import id from '../src/i18n/id.json';
import ja from '../src/i18n/ja.json';

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

describe('the line of a removed entry', () => {
  const DOC_LINT = [
    'doc_lint_failed',
    'doc_lint_base_not_clean',
    'doc_lint_unavailable',
    'doc_lint_repo_config_invalid',
  ] as const;
  const OLDER = ['dequeued', 'checks_failed', 'conflict', 'head_changed', 'stalled', 'not_open'] as const;

  function removed(reason: string | null): QueueEntry {
    return { state: 'removed', reason, position: null } as QueueEntry;
  }

  it.each([
    ['ja', ja],
    ['en', en],
    ['id', id],
  ])('%s says why it left and what to do next for every reason', async (lng, resources) => {
    const i = i18next.createInstance();
    await i.init({
      lng,
      resources: { [lng]: { translation: resources } },
      interpolation: { escapeValue: false },
    });
    const other = reasonLabel(i.t, 'a_reason_of_a_later_server');
    const fix = reasonNext(i.t, null);
    for (const reason of [...OLDER, ...DOC_LINT]) {
      const why = reasonLabel(i.t, reason);
      const next = reasonNext(i.t, reason);
      expect(why).not.toContain('mergeQueue.');
      expect(next).not.toContain('mergeQueue.');
      // Each known reason has its own words (never the generic line).
      expect(why).not.toBe(other);
      const line = entryLine(i.t, removed(reason));
      expect(line).toContain(why);
      expect(line).toContain(next);
      expect(line).not.toContain('{{');
    }
    // The document lint's four reasons each say their own next step; the
    // older ones keep "fix it, then add it again".
    expect(new Set(DOC_LINT.map((r) => reasonNext(i.t, r))).size).toBe(4);
    for (const reason of DOC_LINT) expect(reasonNext(i.t, reason)).not.toBe(fix);
    for (const reason of OLDER) expect(reasonNext(i.t, reason)).toBe(fix);
    expect(reasonNext(i.t, 'doc_lint_failed')).toContain('forge/doc-lint');
    expect(reasonLabel(i.t, 'doc_lint_repo_config_invalid')).toContain('.forge/repo.yml');
    // A reason this build does not know still reads as a whole line.
    expect(entryLine(i.t, removed('a_reason_of_a_later_server'))).toContain(other);
    expect(entryLine(i.t, removed(null))).toContain(fix);
  });
});
