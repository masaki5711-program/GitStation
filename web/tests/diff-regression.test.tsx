// DiffView is one renderer for commits, comparisons and merge requests. The
// merge request screen adds line annotations (U1 phase 4); these snapshots,
// taken before that change, prove the commit and compare screens draw exactly
// the same markup without them (folding and the row budget included).
import { fireEvent, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { FOLD_ROWS } from '../src/features/DiffView';
import { mockServer } from './mock';
import { C1, COMMIT_DETAIL, COMPARE, DIFF, PATCH, repoRoutes } from './repo-fixtures';
import { json, renderApp } from './render';

const BASE = '/acme/forge-web/-';

/** A patch of `n` added lines (past FOLD_ROWS: folded until asked for). */
function bigPatch(n: number): string {
  const lines = [`@@ -0,0 +1,${n} @@`];
  for (let i = 1; i <= n; i += 1) lines.push(`+line ${i}`);
  return lines.join('\n');
}

const RICH_DIFF = {
  truncated: true,
  files: [
    ...DIFF.files,
    { ...DIFF.files[0], old_path: null, new_path: 'docs/new.md', status: 'added', patch: '@@ -0,0 +1,1 @@\n+hello\n\\ No newline at end of file' },
    { ...DIFF.files[0], old_path: 'old.txt', new_path: null, status: 'deleted', patch: '@@ -1 +0,0 @@\n-bye' },
    { ...DIFF.files[0], old_path: 'a.txt', new_path: 'b.txt', status: 'renamed', similarity: 90, patch: '' },
    { ...DIFF.files[0], old_path: 'big.txt', new_path: 'big.txt', patch: bigPatch(FOLD_ROWS + 5) },
    { ...DIFF.files[0], old_path: 'src/x.rs', new_path: 'src/x.rs', patch: PATCH },
  ],
};

/** The diff's markup with React's generated ids replaced (they count every
 * useId of the page, so they say nothing about the diff itself). */
function markup(): string {
  return screen
    .getByTestId('diff')
    .outerHTML.replace(/«[^»]*»|:r[0-9a-z]+:|_r_[0-9a-z]+_/g, '«id»');
}

describe('DiffView output is unchanged for commits and comparisons', () => {
  it('commit screen', async () => {
    const server = mockServer(
      repoRoutes({
        [`GET /api/v1/projects/7/repository/commits/${C1}`]: () =>
          json(200, { ...COMMIT_DETAIL, diff: RICH_DIFF }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/commit/${C1}`, lang: 'en' });
    await screen.findByTestId('diff');
    expect(markup()).toMatchSnapshot();
    // Unfolding the big file draws its rows the same way too.
    fireEvent.click(screen.getByRole('button', { name: /Show the diff/ }));
    expect(markup()).toMatchSnapshot();
  });

  it('compare screen', async () => {
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/compare': () => json(200, { ...COMPARE, diff: RICH_DIFF }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/compare/main...dev`, lang: 'ja' });
    await screen.findByTestId('diff');
    expect(markup()).toMatchSnapshot();
  });
});
