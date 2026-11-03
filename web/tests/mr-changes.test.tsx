// The Changes tab of a merge request (U1 phase 4): the latest version's diff
// with a comment button on every line, the posted position, the threads under
// their lines, and the purged / no-version / folded / failed states.
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { FOLD_ROWS } from '../src/features/DiffView';
import { apiError, mockServer, SESSION } from './mock';
import {
  MR_API,
  MR_SCREENS,
  discussion,
  discussions,
  mrRoutes,
  versionDiff,
  version,
  versions,
} from './mr-fixtures';
import { DIFF } from './repo-fixtures';
import { json, renderApp } from './render';

const CHANGES = `${MR_SCREENS}/pull/3/changes`;
const DIFF_2 = `${MR_API}/3/versions/2/diff`;

function bigPatch(n: number): string {
  const lines = [`@@ -0,0 +1,${n} @@`];
  for (let i = 1; i <= n; i += 1) lines.push(`+line ${i}`);
  return lines.join('\n');
}

const RENAMED = {
  ...DIFF.files[0],
  status: 'renamed',
  old_path: 'a.txt',
  new_path: 'b.txt',
  similarity: 80,
  patch: '@@ -1 +1 @@\n-old text\n+new text',
};

const routes = (over: Parameters<typeof mrRoutes>[0] = {}) =>
  mrRoutes({
    [`GET ${MR_API}/3/versions`]: () => json(200, versions([version(1), version(2)])),
    [`GET ${DIFF_2}`]: () => json(200, versionDiff(2, { ...DIFF, files: [DIFF.files[0], RENAMED] })),
    ...over,
  });

async function open(over: Parameters<typeof mrRoutes>[0] = {}) {
  const server = mockServer(routes(over));
  await renderApp({ fetch: server.fetch, path: CHANGES, lang: 'en' });
  await screen.findByRole('heading', { level: 1, name: /Synthetic change 3/ });
  return server;
}

const commentButton = (name: string) => screen.findByRole('button', { name });

describe('merge request changes', () => {
  it("shows the latest version's diff only", async () => {
    const server = await open();
    expect(await screen.findByText('Version 2 (the latest)')).toBeTruthy();
    expect(await screen.findByRole('region', { name: /src\/lib\.rs/ })).toBeTruthy();
    expect(server.to('GET', DIFF_2)).toHaveLength(1);
    expect(server.calls.some((c) => c.path.endsWith('/versions/1/diff'))).toBe(false);
    expect(server.unmatched).toEqual([]);
  });

  it('every line has a keyboard-reachable comment button naming the path and line', async () => {
    await open();
    await screen.findByRole('region', { name: /src\/lib\.rs/ });
    const names = screen
      .getAllByRole('button', { name: /^Comment on/ })
      .map((b) => b.getAttribute('aria-label'));
    expect(names).toEqual([
      'Comment on line 1 of src/lib.rs',
      'Comment on removed line 2 of src/lib.rs',
      'Comment on line 2 of src/lib.rs',
      'Comment on line 3 of src/lib.rs',
      'Comment on line 4 of src/lib.rs',
      'Comment on removed line 1 of a.txt',
      'Comment on line 1 of b.txt',
    ]);
    for (const b of screen.getAllByRole('button', { name: /^Comment on/ })) {
      expect(b.tagName).toBe('BUTTON');
      expect(b.getAttribute('tabindex')).toBeNull();
    }
  });

  it.each([
    ['Comment on removed line 2 of src/lib.rs', { path: 'src/lib.rs', line: 2, side: 'old' }],
    ['Comment on line 3 of src/lib.rs', { path: 'src/lib.rs', line: 3, side: 'new' }],
    ['Comment on line 4 of src/lib.rs', { path: 'src/lib.rs', line: 4, side: 'new' }],
    ['Comment on removed line 1 of a.txt', { path: 'a.txt', line: 1, side: 'old' }],
    ['Comment on line 1 of b.txt', { path: 'b.txt', line: 1, side: 'new' }],
  ])('%s posts that position on the shown version', async (button, where) => {
    let list: unknown[] = [];
    const server = await open({
      [`GET ${MR_API}/3/discussions`]: () => json(200, discussions(list)),
      [`POST ${MR_API}/3/discussions`]: (c) => {
        const input = c.body as { body: string; position: unknown };
        list = [discussion(9, { kind: 'inline', position: input.position, notes: [{ id: 90, author_id: 5, author: { username: 'taro', display_name: '山田 太郎' }, body: input.body, created_at: '2026-10-22T00:00:00Z' }] })];
        return json(201, list[0]);
      },
    });
    fireEvent.click(await commentButton(button));
    const box = screen.getByLabelText(`Your comment: ${button.replace('Comment on ', '')}`);
    expect(document.activeElement).toBe(box);
    fireEvent.change(box, { target: { value: 'Why this?' } });
    fireEvent.click(screen.getByRole('button', { name: 'Comment' }));
    const thread = await screen.findByTestId('thread-9');
    const post = server.to('POST', `${MR_API}/3/discussions`)[0];
    expect(post?.body).toEqual({ body: 'Why this?', position: { ...where, version: 2 } });
    expect(post?.headers.get('X-GitStation-CSRF')).toBe(SESSION.csrf_token);
    expect(await within(thread).findByText('Why this?')).toBeTruthy();
    // The box closed after the server took the comment.
    expect(screen.queryByLabelText(/^Your comment/)).toBeNull();
  });

  it('existing threads of this version render under their line', async () => {
    await open({
      [`GET ${MR_API}/3/discussions`]: () =>
        json(
          200,
          discussions([
            discussion(9, {
              kind: 'inline',
              position: { path: 'src/lib.rs', line: 3, side: 'new', version: 2 },
            }),
            discussion(8, {
              kind: 'inline',
              position: { path: 'src/lib.rs', line: 2, side: 'old', version: 2 },
            }),
            discussion(7, {
              kind: 'inline',
              position: { path: 'src/lib.rs', line: 3, side: 'new', version: 1 },
            }),
            discussion(6),
          ]),
        ),
    });
    const under3 = await screen.findByTestId('thread-9');
    const line3 = under3.closest('tr')?.previousElementSibling;
    expect(line3?.textContent).toContain('a + b');
    const under2 = screen.getByTestId('thread-8');
    expect(under2.closest('tr')?.previousElementSibling?.textContent).toContain('a - b');
    // Another version's thread and a general one are on the Overview only.
    expect(screen.queryByTestId('thread-7')).toBeNull();
    expect(screen.queryByTestId('thread-6')).toBeNull();
  });

  it('a refused line comment shows the user_message in place and keeps the text', async () => {
    await open({
      [`POST ${MR_API}/3/discussions`]: () =>
        apiError(422, 'その行にはコメントできません', 'req-pos-1', 'FG-REVIEW-0004'),
    });
    fireEvent.click(await commentButton('Comment on line 3 of src/lib.rs'));
    const box = screen.getByLabelText<HTMLTextAreaElement>('Your comment: line 3 of src/lib.rs');
    fireEvent.change(box, { target: { value: 'kept' } });
    fireEvent.click(screen.getByRole('button', { name: 'Comment' }));
    const alert = await screen.findByRole('alert');
    expect(within(alert).getByText('その行にはコメントできません')).toBeTruthy();
    expect(within(alert).getByTestId('request-id').textContent).toBe('req-pos-1');
    expect(box.value).toBe('kept');
  });

  it('cancel closes the comment box', async () => {
    await open();
    fireEvent.click(await commentButton('Comment on line 3 of src/lib.rs'));
    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(screen.queryByLabelText(/^Your comment/)).toBeNull();
  });

  it('a folded file has no comment buttons until it is shown', async () => {
    await open({
      [`GET ${DIFF_2}`]: () =>
        json(
          200,
          versionDiff(2, {
            truncated: false,
            files: [{ ...DIFF.files[0], old_path: null, new_path: 'big.txt', status: 'added', patch: bigPatch(FOLD_ROWS + 1) }],
          }),
        ),
    });
    const show = await screen.findByRole('button', { name: /Show the diff/ });
    expect(document.querySelectorAll('button[aria-label^="Comment on"]')).toHaveLength(0);
    fireEvent.click(show);
    await waitFor(() => {
      // (By attribute: computing 601 accessible names is slow in jsdom.)
      expect(
        document.querySelectorAll('button[aria-label^="Comment on line"][aria-label$="of big.txt"]'),
      ).toHaveLength(FOLD_ROWS + 1);
    });
  });

  it('a purged version shows only the explanation', async () => {
    const server = await open({
      [`GET ${MR_API}/3/versions`]: () =>
        json(200, versions([version(1), version(2, { refs_purged: true })])),
    });
    expect(
      await screen.findByText(
        'The changes of this version were removed (for example, to delete a secret). They can no longer be shown.',
      ),
    ).toBeTruthy();
    expect(screen.queryByTestId('diff')).toBeNull();
    expect(server.calls.some((c) => c.path.includes('/diff'))).toBe(false);
  });

  it('no version yet: says when the changes appear', async () => {
    const server = await open({ [`GET ${MR_API}/3/versions`]: () => json(200, versions([])) });
    expect(
      await screen.findByText('No version is recorded yet. The changes appear here once the push has been read.'),
    ).toBeTruthy();
    expect(server.calls.some((c) => c.path.includes('/diff'))).toBe(false);
  });

  it('a failed diff read shows user_message + request_id and retries', async () => {
    let fail = true;
    await open({
      [`GET ${DIFF_2}`]: () =>
        fail ? apiError(503, '差分を読めません', 'req-diff-1') : json(200, versionDiff(2, DIFF)),
    });
    const alert = await screen.findByRole('alert');
    expect(within(alert).getByText('差分を読めません')).toBeTruthy();
    expect(within(alert).getByTestId('request-id').textContent).toBe('req-diff-1');
    fail = false;
    fireEvent.click(within(alert).getByRole('button', { name: 'Try again' }));
    expect(await screen.findByRole('region', { name: /src\/lib\.rs/ })).toBeTruthy();
  });

  it('a failed version list read shows its error', async () => {
    await open({
      [`GET ${MR_API}/3/versions`]: () => apiError(500, '版を読めません', 'req-v-1'),
    });
    const alert = await screen.findByRole('alert');
    expect(within(alert).getByText('版を読めません')).toBeTruthy();
  });
});
