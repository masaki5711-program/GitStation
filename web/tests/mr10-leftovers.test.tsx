// MR10 review leftovers (MR11 step 0): a persistent status for "Show more",
// focus kept after the last click, link tab order, the window reset.
import { fireEvent, screen, waitFor } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { mockServer } from './mock';
import { json, renderApp } from './render';
import { blob, repoRoutes } from './repo-fixtures';

const BASE = '/acme/forge-web/-';

function big(n: number) {
  return Array.from({ length: n }, (_, i) => `line ${i}`).join('\n');
}

describe('MR10 review leftovers', () => {
  it('"Show more" keeps one status region and moves focus to the first new row on the last click', async () => {
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/blob': () => json(200, blob('big.txt', big(3500))),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/blob/main/big.txt`, lang: 'en' });
    const code = await screen.findByTestId('code');
    const status = screen.getAllByRole('status').find((s) => s.textContent.includes('Showing'));
    expect(status?.textContent).toBe('Showing 3000 of 3500 lines.');
    fireEvent.click(screen.getByRole('button', { name: /Show more/ }));
    await waitFor(() => {
      expect(code.querySelectorAll('tr')).toHaveLength(3500);
    });
    // The same status element survives, now empty; the button is gone.
    expect(status?.isConnected).toBe(true);
    expect(status?.textContent).toBe('');
    expect(screen.queryByRole('button', { name: /Show more/ })).toBeNull();
    await waitFor(() => {
      expect(document.activeElement?.id).toBe('L3001');
    });
  });

  it('line-number links are not tab stops; the "go to line" control is the entry', async () => {
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/blob': () => json(200, blob('a.txt', big(20))),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/blob/main/a.txt`, lang: 'en' });
    const code = await screen.findByTestId('code');
    for (const a of code.querySelectorAll('th a')) expect(a.getAttribute('tabindex')).toBe('-1');
    const input = screen.getByLabelText('Go to line');
    fireEvent.change(input, { target: { value: '7' } });
    fireEvent.click(screen.getByRole('button', { name: 'Go' }));
    expect(window.location.hash).toBe('#L7');
  });

  it('the truncation notices are status regions', async () => {
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/blob': () =>
          json(200, blob('t.txt', 'x', { truncated: true })),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/blob/main/t.txt`, lang: 'en' });
    const note = await screen.findByTestId('blob-truncated');
    expect(note.getAttribute('role')).toBe('status');
  });
});
