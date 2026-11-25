// The project's Wiki tab (plan 20 W4, ADR-0040 §5): the wiki is an ordinary
// linked project, so its pages are documents (the document API), saved by
// the simple editor's one write (a direct commit to `main`). Synthetic data
// only.
import axe from 'axe-core';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { PROJECT, apiError, mockServer, type Call, type Handler } from './mock';
import { json, renderApp } from './render';
import { repoRoutes } from './repo-fixtures';

const HEAD = 'c'.repeat(40);
const NEW_HEAD = 'd'.repeat(40);
const WIKI_ID = 21;
const LINK = {
  wiki_project_id: WIKI_ID,
  wiki_path: 'acme/forge-web-wiki',
  display_name: 'Forge Web Wiki',
  default_branch: 'main',
};
const WIKI_PROJECT = {
  ...PROJECT,
  id: WIKI_ID,
  path: 'acme/forge-web-wiki',
  name: 'forge-web-wiki',
  display_name: 'Forge Web Wiki',
  description: '',
  role: 'developer',
};
const REV = { commit: HEAD, full_name: 'refs/heads/main', name: 'main', type: 'branch' };

const summary = (id: string, title: string, path: string) => ({
  classification: 'internal',
  doctype: 'wiki.page',
  id,
  path,
  title,
  translations: [],
});

const LIST = {
  documents: [
    summary('WIKI-0001', 'Forge Web Wiki', 'WIKI-0001.md'),
    summary('WIKI-0002', '点検の手順', 'procedures/WIKI-0002.md'),
  ],
  incomplete: null,
  next_after: null,
  ref: REV,
  withheld: null,
};

const FIRST_PAGE = [
  '---',
  'doctype: wiki.page',
  'id: WIKI-0001',
  'title: "Forge Web Wiki"',
  'owner: "acme"',
  'classification: internal',
  'reviewed_by: "hanako"',
  '---',
  '',
  '## この Wiki について',
  '',
  'プロジェクトの説明をここに書きます。',
  '',
].join('\n');

const document = (id: string, title: string, path: string, problems = 0) => ({
  classification: 'internal',
  doctype: 'wiki.page',
  fields: [],
  id,
  incomplete: null,
  lang: null,
  owner: 'acme',
  path,
  problem_count: problems,
  problems: problems > 0 ? [{ line: 9, rule: 'broken_ref', subject: 'WIKI-0099' }] : [],
  ref: REV,
  refs: [],
  sections: [
    {
      heading: 'この Wiki について',
      line: 10,
      pieces: [
        { block: null, display: 'プロジェクトの説明をここに書きます。', line: 12, markdown: 'プロジェクトの説明をここに書きます。', type: 'text' },
      ],
    },
  ],
  title,
  translations: [],
  version: 1,
});

const NO_CHECKS = { check_runs: [], head_sha: HEAD, schema_version: 1 };

const commitAnswer = (path: string) =>
  json(201, {
    blob_sha: 'e'.repeat(40),
    branch: 'main',
    branch_created: false,
    commit_sha: NEW_HEAD,
    merge_request: null,
    merge_request_error: null,
    parent_sha: HEAD,
    path,
  });

/** The owner project (7, a Maintainer here) with its linked wiki (21). */
function wikiRoutes(over: Record<string, Handler> = {}): Record<string, Handler> {
  return repoRoutes({
    'GET /api/v1/projects/7': () => json(200, { ...PROJECT, role: 'maintainer' }),
    'GET /api/v1/projects/7/wiki': () => json(200, LINK),
    'GET /api/v1/projects/21': () => json(200, WIKI_PROJECT),
    'GET /api/v1/projects/21/documents': () => json(200, LIST),
    [`GET /api/v1/projects/21/commits/${HEAD}/checks`]: () => json(200, NO_CHECKS),
    'GET /api/v1/projects/21/documents/WIKI-0001': () =>
      json(200, document('WIKI-0001', 'Forge Web Wiki', 'WIKI-0001.md')),
    'GET /api/v1/projects/21/repository/blob': (c) =>
      c.query.get('path') === 'WIKI-0001.md'
        ? json(200, {
            binary: false,
            content: FIRST_PAGE,
            mode: '100644',
            name: 'WIKI-0001.md',
            oid: 'e'.repeat(40),
            path: 'WIKI-0001.md',
            ref: REV,
            size: FIRST_PAGE.length,
            truncated: false,
            type: 'blob',
          })
        : apiError(404, 'ファイルが見つかりません。', 'req-blob', 'FG-REPO-0023'),
    ...over,
  });
}

const posted = (calls: Call[]) =>
  calls.filter(
    (c) => c.method === 'POST' && c.path === '/api/v1/projects/21/repository/commits/single-file',
  );

describe('project wiki tab', () => {
  it('create_wiki_then_first_page', async () => {
    let linked = false;
    let documents = { ...LIST, documents: [summary('WIKI-0001', 'Forge Web Wiki', 'WIKI-0001.md')] };
    const server = mockServer(
      wikiRoutes({
        'GET /api/v1/projects/7/wiki': () =>
          linked ? json(200, LINK) : apiError(404, '見つかりません。', 'req-nowiki', 'FG-REPO-0005'),
        'POST /api/v1/projects/7/wiki': () => {
          linked = true;
          return json(201, {
            ...LINK,
            project_id: 7,
            commit: HEAD,
            first_page: 'WIKI-0001.md',
          });
        },
        'GET /api/v1/projects/21/documents': () => json(200, documents),
        'POST /api/v1/projects/21/repository/commits/single-file': () => {
          documents = {
            ...documents,
            documents: [...documents.documents, summary('WIKI-0002', '設備の点検手順', 'WIKI-0002.md')],
          };
          return commitAnswer('WIKI-0002.md');
        },
        'GET /api/v1/projects/21/documents/WIKI-0002': () =>
          json(200, document('WIKI-0002', '設備の点検手順', 'WIKI-0002.md')),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/wiki' });
    // Empty state: why, and the one thing to press.
    expect(await screen.findByText('このプロジェクトにはまだ Wiki がありません。')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Wiki を作る' }));
    // Created: the page list shows the first page.
    expect(await screen.findByRole('link', { name: 'Forge Web Wiki' })).toBeTruthy();
    expect(server.to('POST', '/api/v1/projects/7/wiki')).toHaveLength(1);
    // The first own page.
    fireEvent.click(screen.getByRole('link', { name: 'ページを作る' }));
    fireEvent.change(await screen.findByLabelText('タイトル'), { target: { value: '設備の点検手順' } });
    fireEvent.change(screen.getByLabelText('本文'), { target: { value: '## 手順\n\n電源を切る。\n' } });
    fireEvent.click(screen.getByRole('button', { name: '保存する' }));
    expect(await screen.findByRole('heading', { level: 1, name: '設備の点検手順' })).toBeTruthy();
    const sent = posted(server.calls);
    expect(sent).toHaveLength(1);
    const body = sent[0]?.body as Record<string, unknown>;
    expect(body.path).toBe('WIKI-0002.md');
    expect(body.branch).toBe('main');
    expect(body.start_sha).toBe(HEAD);
    expect(body.create_branch).toBeUndefined();
    expect(body.content).toBe(
      [
        '---',
        'doctype: wiki.page',
        'id: WIKI-0002',
        'title: "設備の点検手順"',
        'owner: "acme"',
        'classification: internal',
        '---',
        '',
        '## 手順',
        '',
        '電源を切る。',
        '',
      ].join('\n'),
    );
    expect(server.unmatched).toEqual([]);
  });

  it('a Developer of the owner sees no create button: who may create it is said instead', async () => {
    const server = mockServer(
      wikiRoutes({
        'GET /api/v1/projects/7': () => json(200, PROJECT),
        'GET /api/v1/projects/7/wiki': () => apiError(404, '見つかりません。', 'req-nowiki', 'FG-REPO-0005'),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/wiki' });
    expect(await screen.findByText('このプロジェクトにはまだ Wiki がありません。')).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Wiki を作る' })).toBeNull();
    expect(screen.getByText('Wiki はメンテナー以上の人が作れます。')).toBeTruthy();
  });

  it('non_engineer_edits_without_yaml', async () => {
    const server = mockServer(
      wikiRoutes({
        'POST /api/v1/projects/21/repository/commits/single-file': () => commitAnswer('WIKI-0001.md'),
      }),
    );
    const { container } = await renderApp({
      fetch: server.fetch,
      path: '/acme/forge-web/-/wiki/WIKI-0001/edit',
    });
    const title = await screen.findByLabelText('タイトル');
    expect((title as HTMLInputElement).value).toBe('Forge Web Wiki');
    const text = screen.getByLabelText<HTMLTextAreaElement>('本文');
    expect(text.value).toBe('## この Wiki について\n\nプロジェクトの説明をここに書きます。\n');
    // No YAML anywhere on the screen.
    expect(container.textContent).not.toContain('doctype');
    expect(container.textContent).not.toContain('---');
    expect(container.textContent).not.toContain('reviewed_by');
    for (const box of screen.getAllByRole('textbox')) {
      expect((box as HTMLInputElement).value).not.toContain('doctype:');
    }
    fireEvent.change(title, { target: { value: 'Forge Web の手引き' } });
    fireEvent.change(screen.getByLabelText('情報の区分'), { target: { value: 'public' } });
    fireEvent.change(text, { target: { value: '## はじめに\n\n読んでください。\n' } });
    fireEvent.click(screen.getByRole('button', { name: '保存する' }));
    await waitFor(() => {
      expect(posted(server.calls)).toHaveLength(1);
    });
    const body = posted(server.calls)[0]?.body as Record<string, unknown>;
    expect(body.path).toBe('WIKI-0001.md');
    expect(body.start_sha).toBe(HEAD);
    // The id, the owner and a field the form does not show are kept.
    expect(body.content).toBe(
      [
        '---',
        'doctype: wiki.page',
        'id: WIKI-0001',
        'title: "Forge Web の手引き"',
        'owner: "acme"',
        'classification: public',
        'reviewed_by: "hanako"',
        '---',
        '',
        '## はじめに',
        '',
        '読んでください。',
        '',
      ].join('\n'),
    );
    expect(server.unmatched).toEqual([]);
  });

  it('lint_red_shows_next_step', async () => {
    const server = mockServer(
      wikiRoutes({
        [`GET /api/v1/projects/21/commits/${HEAD}/checks`]: () =>
          json(200, {
            head_sha: HEAD,
            schema_version: 1,
            check_runs: [
              {
                annotations: [
                  {
                    end_line: 8,
                    level: 'failure',
                    message: 'broken reference [[WIKI-0099]]',
                    path: 'procedures/WIKI-0002.md',
                    start_line: 8,
                    title: null,
                  },
                ],
                completed_at: '2026-11-25T01:00:00Z',
                conclusion: 'failure',
                created_at: '2026-11-25T01:00:00Z',
                details_url: null,
                head_sha: HEAD,
                id: 1,
                name: 'forge/doc-lint',
                reported_by: null,
                revision: 1,
                status: 'completed',
                summary: '1 problem',
                updated_at: '2026-11-25T01:00:00Z',
              },
            ],
          }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/wiki' });
    const line = await screen.findByTestId('wiki-lint');
    // One line: why it is stopped and what to press next.
    expect(line.textContent).toContain('点検の手順');
    expect(line.textContent).toContain('8 行目');
    const fix = within(line).getByRole('link', { name: '直す' });
    expect(fix.getAttribute('href')).toBe('/acme/forge-web/-/wiki/WIKI-0002/edit');
    expect(server.unmatched).toEqual([]);
  });

  it('a page whose own text has problems says so with the fix link', async () => {
    const server = mockServer(
      wikiRoutes({
        'GET /api/v1/projects/21/documents/WIKI-0001': () =>
          json(200, document('WIKI-0001', 'Forge Web Wiki', 'WIKI-0001.md', 1)),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/wiki/WIKI-0001' });
    expect(await screen.findByRole('heading', { level: 1, name: 'Forge Web Wiki' })).toBeTruthy();
    const line = screen.getByTestId('wiki-lint');
    expect(line.textContent).toContain('9 行目');
    expect(within(line).getByRole('link', { name: '直す' }).getAttribute('href')).toBe(
      '/acme/forge-web/-/wiki/WIKI-0001/edit',
    );
  });

  it('lists the pages as a folder tree and shows one page', async () => {
    const server = mockServer(wikiRoutes());
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/wiki', lang: 'en' });
    const tree = await screen.findByRole('navigation', { name: 'Pages' });
    const folder = within(tree).getByText('procedures');
    expect(folder).toBeTruthy();
    expect(within(tree).getByRole('link', { name: '点検の手順' }).getAttribute('href')).toBe(
      '/acme/forge-web/-/wiki/WIKI-0002',
    );
    fireEvent.click(within(tree).getByRole('link', { name: 'Forge Web Wiki' }));
    expect(await screen.findByRole('heading', { level: 1, name: 'Forge Web Wiki' })).toBeTruthy();
    expect(await screen.findByText('プロジェクトの説明をここに書きます。')).toBeTruthy();
    expect(screen.getByRole('link', { name: 'Edit' }).getAttribute('href')).toBe(
      '/acme/forge-web/-/wiki/WIKI-0001/edit',
    );
    expect(screen.getByRole('link', { name: 'History' }).getAttribute('href')).toBe(
      '/acme/forge-web/-/wiki/WIKI-0001/history',
    );
    expect(server.unmatched).toEqual([]);
  });

  it('shows a page history from the commits of its file', async () => {
    const server = mockServer(
      wikiRoutes({
        'GET /api/v1/projects/21/repository/commits': (c) => {
          expect(c.query.get('path')).toBe('WIKI-0001.md');
          return json(200, {
            commits: [
              {
                author_email: 'hanako@example.invalid',
                author_name: 'Hanako Sato',
                authored_at: '2026-11-24T10:00:00+09:00',
                committed_at: '2026-11-24T10:00:00+09:00',
                committer_email: 'hanako@example.invalid',
                committer_name: 'Hanako Sato',
                oid: HEAD,
                parents: [],
                subject: 'Wiki: Forge Web Wiki を直す',
              },
            ],
            has_more: false,
            head: HEAD,
            next_cursor: null,
            path: 'WIKI-0001.md',
            ref: REV,
          });
        },
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/wiki/WIKI-0001/history' });
    expect(await screen.findByText('Wiki: Forge Web Wiki を直す')).toBeTruthy();
    expect(screen.getByText('Hanako Sato')).toBeTruthy();
    expect(server.unmatched).toEqual([]);
  });

  it('a Reporter reads but is not offered editing', async () => {
    const server = mockServer(
      wikiRoutes({ 'GET /api/v1/projects/21': () => json(200, { ...WIKI_PROJECT, role: 'reporter' }) }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/wiki/WIKI-0001' });
    expect(await screen.findByRole('heading', { level: 1, name: 'Forge Web Wiki' })).toBeTruthy();
    expect(screen.queryByRole('link', { name: '編集する' })).toBeNull();
    expect(screen.queryByRole('link', { name: 'ページを作る' })).toBeNull();
  });

  it('a conflict on save says to reload, with the request id', async () => {
    const server = mockServer(
      wikiRoutes({
        'POST /api/v1/projects/21/repository/commits/single-file': () =>
          apiError(409, 'ほかの人が先に保存しました。', 'req-conflict', 'FG-GIT-0027'),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/wiki/WIKI-0001/edit' });
    fireEvent.change(await screen.findByLabelText('本文'), { target: { value: 'x\n' } });
    fireEvent.click(screen.getByRole('button', { name: '保存する' }));
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('ほかの人が先に保存しました。');
    expect(alert.textContent).toContain('req-conflict');
    expect(alert.textContent).not.toContain('FG-GIT-0027');
  });
});

async function violations(root: HTMLElement): Promise<string[]> {
  const result = await axe.run(root, {
    runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
    rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
  });
  return result.violations.map((v) => `${v.id}: ${v.help}`);
}

const EMPTY_REASON = {
  ja: 'このプロジェクトにはまだ Wiki がありません。',
  en: 'This project has no wiki yet.',
  id: 'Proyek ini belum memiliki wiki.',
} as const;

describe('wiki screens are accessible', () => {
  for (const lang of ['ja', 'en', 'id'] as const) {
    it(`has no axe violations (${lang})`, async () => {
      const home = await renderApp({ fetch: mockServer(wikiRoutes()).fetch, path: '/acme/forge-web/-/wiki', lang });
      await screen.findAllByRole('link', { name: '点検の手順' });
      expect(await violations(home.container)).toEqual([]);
      home.unmount();
      const form = await renderApp({
        fetch: mockServer(wikiRoutes()).fetch,
        path: '/acme/forge-web/-/wiki/WIKI-0001/edit',
        lang,
      });
      await screen.findByDisplayValue('Forge Web Wiki');
      expect(await violations(form.container)).toEqual([]);
      form.unmount();
      const empty = await renderApp({
        fetch: mockServer(
          wikiRoutes({
            'GET /api/v1/projects/7/wiki': () => apiError(404, 'x', 'req-nowiki', 'FG-REPO-0005'),
          }),
        ).fetch,
        path: '/acme/forge-web/-/wiki',
        lang,
      });
      await screen.findByText(EMPTY_REASON[lang]);
      expect(await violations(empty.container)).toEqual([]);
    });
  }
});
