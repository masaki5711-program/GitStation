// The AI asset draft on the merge request screen (plan 17 P17-10; ADR-0039
// amendment P17-10b): only for a merge request from `improve/<id>`,
// labelled 「AI の下書き（提案）」 in words (never the colour alone), the
// draft branch and its commit as links, the files it changes, the count of
// dropped edits, a moved or deleted branch said so, and loading / empty /
// error (user_message and 問い合わせ番号) states. Synthetic data.
import axe from 'axe-core';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { AI_DRAFT_POLL_MS, aiDraftPollInterval, isImprovementBranch } from '../src/api/aiDraft';
import type { Language } from '../src/i18n';
import { apiError, mockServer, type Handler } from './mock';
import { MR_API, MR_SCREENS, mr, mrRoutes } from './mr-fixtures';
import { json, renderApp } from './render';

const VIEW = `${MR_SCREENS}/pull/3`;
const DRAFT_API = `${MR_API}/3/ai-draft`;
const IMPROVEMENT = mr(3, { source_branch: 'improve/ir-1', draft: true });

function draftBody(over: Record<string, unknown> | null = {}) {
  return {
    iid: 3,
    label: 'AI の下書き（提案） / AI draft (suggestion)',
    improvement: true,
    state: over === null ? 'none' : 'proposed',
    branch: null,
    draft:
      over === null
        ? null
        : {
            branch: 'ai-draft/ir-1',
            commit_sha: 'c'.repeat(40),
            parent_sha: 'b'.repeat(40),
            files: ['skills/pe-quality-spc.md'],
            dropped_edits: 0,
            advice_id: 5,
            proposed_at: '2026-11-28T01:00:00Z',
            branch_state: 'current',
            branch_head: 'c'.repeat(40),
            ...over,
          },
  };
}

async function open(answer: Handler, lang: Language = 'en', merge = IMPROVEMENT) {
  const server = mockServer(
    mrRoutes({
      [`GET ${MR_API}/3`]: () => json(200, merge),
      [`GET ${DRAFT_API}`]: answer,
    }),
  );
  const view = await renderApp({ fetch: server.fetch, path: VIEW, lang });
  await screen.findByRole('heading', { level: 1, name: /Synthetic change 3/ });
  return { server, view };
}

const section = async () => screen.findByTestId('mr-ai-draft');

async function violations(root: HTMLElement): Promise<string[]> {
  const result = await axe.run(root, {
    runOnly: {
      type: 'tag',
      values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'],
    },
    rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
  });
  return result.violations.map((v) => `${v.id}: ${v.help}`);
}

describe('AI draft: when it is asked for', () => {
  it('a merge request from another branch: no section and no request', async () => {
    const { server } = await open(() => json(200, draftBody()), 'en', mr(3));
    await waitFor(() => {
      expect(screen.queryByText(/Loading/)).toBeNull();
    });
    expect(screen.queryByTestId('mr-ai-draft')).toBeNull();
    expect(server.to('GET', DRAFT_API)).toEqual([]);
  });

  it('only improve/<id> branches, and polled only while one waits for its draft', () => {
    expect(isImprovementBranch('improve/ir-1')).toBe(true);
    expect(isImprovementBranch('refs/heads/improve/ir-1')).toBe(true);
    expect(isImprovementBranch('improve/')).toBe(false);
    expect(isImprovementBranch('feature/improve/x')).toBe(false);
    expect(aiDraftPollInterval(undefined, true)).toBe(false);
    expect(aiDraftPollInterval(draftBody(null), true)).toBe(AI_DRAFT_POLL_MS);
    expect(aiDraftPollInterval(draftBody(null), false)).toBe(false);
    expect(aiDraftPollInterval(draftBody(), true)).toBe(false);
  });
});

describe('AI draft: the panel', () => {
  it('none: labelled in words, and "no draft yet"', async () => {
    await open(() => json(200, draftBody(null)));
    const box = await section();
    expect(await within(box).findByTestId('ai-draft-none')).toBeTruthy();
    expect(within(box).getByText('AI draft (suggestion)')).toBeTruthy();
    expect(within(box).getByTestId('ai-draft-only').textContent).toMatch(/suggestion only/);
  });

  it('proposed: the branch, the commit, the files, the dropped count and the next step', async () => {
    await open(() => json(200, draftBody({ dropped_edits: 2 })));
    const box = await section();
    const branch = await within(box).findByTestId('ai-draft-branch');
    expect(branch.getAttribute('href')).toBe('/acme/forge-web/-/tree/ai-draft/ir-1');
    const commit = within(box).getByTestId('ai-draft-commit');
    expect(commit.getAttribute('href')).toBe(`/acme/forge-web/-/commit/${'c'.repeat(40)}`);
    expect(commit.textContent).toBe('c'.repeat(8));
    const files = within(box).getByTestId('ai-draft-files');
    const file = within(files).getByRole('link', {
      name: 'skills/pe-quality-spc.md',
    });
    expect(file.getAttribute('href')).toBe('/acme/forge-web/-/blob/ai-draft/ir-1/skills/pe-quality-spc.md');
    expect(within(box).getByTestId('ai-draft-dropped').textContent).toContain('2 of the model');
    expect(within(box).getByTestId('ai-draft-next').textContent).toContain('improve/ir-1');
    expect(within(box).queryByTestId('ai-draft-moved')).toBeNull();
    expect(await violations(box)).toEqual([]);
  });

  it('ja: 「AI の下書き（提案）」 and no dropped edit said plainly', async () => {
    await open(() => json(200, draftBody()), 'ja');
    const box = await section();
    expect(await within(box).findByText('AI の下書き（提案）')).toBeTruthy();
    expect(within(box).getByTestId('ai-draft-dropped').textContent).toBe('確認で捨てた編集はありません。');
  });

  it('a moved branch names its head; a deleted one is not linked', async () => {
    await open(() => json(200, draftBody({ branch_state: 'moved', branch_head: 'd'.repeat(40) })));
    const box = await section();
    expect((await within(box).findByTestId('ai-draft-moved')).textContent).toContain('d'.repeat(8));
  });

  it('a deleted branch is said so, without links into it', async () => {
    await open(() => json(200, draftBody({ branch_state: 'deleted', branch_head: null })));
    const box = await section();
    expect(await within(box).findByTestId('ai-draft-deleted')).toBeTruthy();
    expect(within(box).getByTestId('ai-draft-branch').tagName).toBe('CODE');
    expect(within(box).queryByTestId('ai-draft-next')).toBeNull();
    expect(within(within(box).getByTestId('ai-draft-files')).queryByRole('link')).toBeNull();
  });

  it('a draft branch without its record is said so, never "no draft"', async () => {
    await open(() =>
      json(200, {
        ...draftBody(null),
        state: 'unrecorded',
        branch: 'ai-draft/ir-1',
      }),
    );
    const box = await section();
    const line = await within(box).findByTestId('ai-draft-unrecorded');
    expect(line.textContent).toContain('could not be read');
    expect(within(line).getByRole('link').getAttribute('href')).toBe('/acme/forge-web/-/tree/ai-draft/ir-1');
    expect(within(box).queryByTestId('ai-draft-none')).toBeNull();
  });

  it('a closed or ready merge request promises no draft', async () => {
    await open(() => json(200, draftBody(null)), 'en', mr(3, { source_branch: 'improve/ir-1', draft: false }));
    const box = await section();
    expect((await within(box).findByTestId('ai-draft-none')).textContent).toBe(
      'No AI draft was made for this merge request.',
    );
  });

  it('a model-made path is plain text, never markup', async () => {
    await open(() => json(200, draftBody({ files: ['skills/<b>x</b>.md'] })));
    const box = await section();
    const files = await within(box).findByTestId('ai-draft-files');
    expect(files.querySelector('b')).toBeNull();
    expect(files.textContent).toContain('skills/<b>x</b>.md');
  });

  it('an error shows the message and the request id, and retries', async () => {
    let n = 0;
    await open(() => {
      n += 1;
      return n === 1 ? apiError(500, 'The AI draft could not be read.', 'req-draft-1') : json(200, draftBody(null));
    });
    const box = await section();
    expect(await within(box).findByText('The AI draft could not be read.')).toBeTruthy();
    expect(box.textContent).toContain('req-draft-1');
    fireEvent.click(within(box).getByRole('button', { name: /Try again|Retry/ }));
    expect(await within(await section()).findByTestId('ai-draft-none')).toBeTruthy();
  });
});
