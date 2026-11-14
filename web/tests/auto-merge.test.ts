// M4 C9: the merge request's one auto-merge line — why it stopped and what
// to press next — in every language.
import i18next from 'i18next';
import { describe, expect, it } from 'vitest';

import type { AutoMergeState, MergeRequestAutoMerge } from '../src/api/autoMerge';
import { lineKind, nextText, stateKind, whyText, type LineKind } from '../src/features/mr/AutoMergeLine';
import en from '../src/i18n/en.json';
import id from '../src/i18n/id.json';
import ja from '../src/i18n/ja.json';

function state(over: Partial<AutoMergeState>): AutoMergeState {
  return {
    state: 'waiting',
    class: null,
    reason: 'risk_pending',
    detail: null,
    error_code: null,
    head_sha: 'a'.repeat(40),
    request_id: 'req-1',
    changed_at: '2026-11-13T00:00:00Z',
    checked_at: '2026-11-13T00:00:00Z',
    ...over,
  };
}

function mr(over: Partial<MergeRequestAutoMerge>): MergeRequestAutoMerge {
  return { iid: 1, mode: 'full', acts: true, stop: null, auto_merge: null, ...over };
}

describe('stateKind', () => {
  it('names each stop class and never shows a warning as a stop', () => {
    const stopped = (cls: string, reason = 'risk_stop') =>
      stateKind(state({ state: 'stopped', class: cls, reason }));
    expect(stopped('data_loss')).toBe('data_loss');
    expect(stopped('leak')).toBe('leak');
    expect(stopped('developer_pc')).toBe('developer_pc');
    expect(stopped('program_breakage')).toBe('breakage_risk');
    expect(stopped('program_breakage', 'required_check_failed')).toBe('breakage_check');
    expect(stopped('program_breakage', 'merge_queue_checks_failed')).toBe('breakage_queue');
    expect(stopped('not_assessed')).toBe('not_assessed');
    expect(stopped('emergency_stop', 'instance_stopped')).toBe('instance_stopped');
    expect(stateKind(state({ state: 'queued', reason: 'queued_with_warning' }))).toBe('queued_warning');
    expect(stateKind(state({ reason: 'protected_path' }))).toBe('protected_path');
    expect(stateKind(state({ state: 'held', reason: 'dequeued_by_person' }))).toBe('held');
  });

  it('shows nothing when the mode does not act, and the emergency stop first', () => {
    expect(lineKind(mr({ acts: false, auto_merge: state({}) }))).toBeNull();
    expect(lineKind(mr({ stop: 'project', auto_merge: state({}) }))).toBe('project_stopped');
    expect(lineKind(mr({}))).toBeNull();
  });
});

describe('the line in every language', () => {
  const kinds: LineKind[] = [
    'instance_stopped',
    'project_stopped',
    'data_loss',
    'leak',
    'developer_pc',
    'breakage_check',
    'breakage_queue',
    'breakage_risk',
    'not_assessed',
    'draft',
    'risk_pending',
    'approvals',
    'protected_path',
    'conflict',
    'waiting',
    'held',
    'queued',
    'queued_warning',
    'queued_by_person',
  ];
  it.each([
    ['ja', ja],
    ['en', en],
    ['id', id],
  ])('%s has a why and a next for every kind', async (lng, resources) => {
    const i = i18next.createInstance();
    await i.init({ lng, resources: { [lng]: { translation: resources } } });
    for (const k of kinds) {
      const why = whyText(i.t, k, 'sql_drop');
      const next = nextText(i.t, k);
      expect(why).not.toContain('autoMerge.');
      expect(next).not.toContain('autoMerge.');
      expect(why.length).toBeGreaterThan(0);
    }
    expect(whyText(i.t, 'data_loss', 'sql_drop')).toContain('sql_drop');
  });
});
