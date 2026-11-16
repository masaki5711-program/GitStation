// M4 C14: the merge request's one autofix line — what was fixed
// automatically or why it was refused — in every language.
import i18next from 'i18next';
import { describe, expect, it } from 'vitest';

import type { AutofixAttempt } from '../src/api/autofix';
import { isLive } from '../src/api/autofix';
import { autofixKind, autofixText, type AutofixKind } from '../src/features/mr/AutofixLine';
import en from '../src/i18n/en.json';
import id from '../src/i18n/id.json';
import ja from '../src/i18n/ja.json';

function attempt(over: Partial<AutofixAttempt>): AutofixAttempt {
  return {
    attempt_id: 3,
    trigger: 'pipeline_failed',
    fixer: 'fmt',
    state: 'running',
    reason: null,
    paths: [],
    commit_sha: null,
    error_code: null,
    head_sha: 'a'.repeat(40),
    request_id: 'req-1',
    updated_at: '2026-11-16T00:00:00Z',
    ...over,
  };
}

describe('autofixKind', () => {
  it('names what happened and stays silent when nothing changed', () => {
    expect(autofixKind(null)).toBeNull();
    expect(autofixKind(attempt({ state: 'queued' }))).toBe('running');
    expect(autofixKind(attempt({ state: 'applied', commit_sha: 'c'.repeat(40) }))).toBe('applied');
    expect(autofixKind(attempt({ state: 'proposed', reason: 'migration' }))).toBe('proposed');
    expect(autofixKind(attempt({ state: 'refused', reason: 'outside_fixer_paths' }))).toBe('refused');
    expect(autofixKind(attempt({ state: 'failed', reason: 'job_failed' }))).toBe('failed');
    expect(autofixKind(attempt({ state: 'skipped', reason: 'denied' }))).toBe('denied');
    expect(autofixKind(attempt({ state: 'skipped', reason: 'disabled' }))).toBeNull();
    expect(autofixKind(attempt({ state: 'no_change' }))).toBeNull();
    expect(autofixKind(attempt({ state: 'superseded' }))).toBeNull();
    expect(isLive(attempt({ state: 'running' }))).toBe(true);
    expect(isLive(attempt({ state: 'applied' }))).toBe(false);
  });
});

describe('the line in every language', () => {
  const kinds: AutofixKind[] = ['running', 'applied', 'proposed', 'refused', 'failed', 'denied'];
  it.each([
    ['ja', ja],
    ['en', en],
    ['id', id],
  ])('%s has a text for every kind with its names', async (lng, resources) => {
    const i = i18next.createInstance();
    await i.init({ lng, resources: { [lng]: { translation: resources } } });
    for (const k of kinds) {
      const text = autofixText(
        i.t,
        k,
        attempt({ paths: [{ path: 'db/migrations/1.sql', reason: 'migration' }], commit_sha: 'c'.repeat(40) }),
      );
      expect(text).not.toContain('autofix.');
      expect(text.length).toBeGreaterThan(0);
    }
    const proposed = autofixText(i.t, 'proposed', attempt({ paths: [{ path: 'db/migrations/1.sql', reason: 'migration' }] }));
    expect(proposed).toContain('db/migrations/1.sql');
    expect(autofixText(i.t, 'applied', attempt({ commit_sha: 'c'.repeat(40) }))).toContain('cccccccc');
    expect(autofixText(i.t, 'refused', attempt({}))).toContain('req-1');
  });
});
