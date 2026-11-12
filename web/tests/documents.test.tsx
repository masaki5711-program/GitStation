// Plan 13 stage 3: the document screens. One document: its summary (id,
// type, owner, classification as a word), sections with headings, the
// prose joined for display, blocks grouped by kind as a table (a deprecated
// row says so), references (found: a link "ID title"; hidden: the id and a
// note, never a title; broken: marked), a translation; the list with paging
// and what an agent's list left out; loading, not found and an error with
// its request id; axe in every language. Synthetic data only.
import axe from 'axe-core';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import type { Language } from '../src/i18n';
import { documentId } from '../src/api/documents';
import { docRefTarget, escapeMarkdown, linkRefs } from '../src/features/docRefs';
import { apiError, mockServer, type Handler } from './mock';
import { repoRoutes } from './repo-fixtures';
import { json, renderApp } from './render';

const C = 'a'.repeat(40);
const REV = { type: 'branch', name: 'main', full_name: 'refs/heads/main', commit: C };
const API = '/api/v1/projects/7/documents';
const BASE = '/acme/forge-web/-/documents';

const ref = (over: Record<string, unknown>) => ({
  repo: null,
  block: null,
  line: 9,
  title: null,
  block_kind: null,
  ...over,
});

const REFS = [
  ref({ doc: 'QP-1', target: 'found', title: 'Synthetic procedure', line: 9 }),
  ref({ doc: 'URS-1', block: 'REQ-2', target: 'hidden', line: 10 }),
  ref({ doc: 'QM-9', target: 'broken', line: 11 }),
];

const DOC = {
  version: 1,
  ref: REV,
  path: 'safety/RA-1.md',
  lang: null,
  translations: ['en'],
  id: 'RA-1',
  doctype: 'safety.risk-assessment',
  title: 'Synthetic assessment',
  owner: 'safety-team',
  classification: 'internal',
  fields: [{ key: 'related', value: ['URS-1'] }],
  sections: [
    {
      heading: 'Scope',
      line: 7,
      pieces: [
        {
          type: 'text',
          line: 9,
          markdown: '対象は[[QP-1]]に従う。\nまた[[URS-1#REQ-2]]と[[QM-9]]。',
          display: '対象は[[QP-1]]に従う。また[[URS-1#REQ-2]]と[[QM-9]]。',
          block: null,
        },
      ],
    },
    {
      heading: 'Hazards',
      line: 12,
      pieces: [
        {
          type: 'block',
          line: 13,
          markdown: null,
          display: null,
          block: {
            kind: 'hazard',
            id: 'H-01',
            attrs: [{ key: 'severity', value: '4' }],
            deprecated: false,
            body: 'A hand\nin the gap.',
            display: 'A hand in the gap.',
          },
        },
        {
          type: 'block',
          line: 17,
          markdown: null,
          display: null,
          block: {
            kind: 'hazard',
            id: 'H-02',
            attrs: [
              { key: 'severity', value: '2' },
              { key: 'status', value: 'deprecated' },
            ],
            deprecated: true,
            body: 'Old hazard.',
            display: 'Old hazard.',
          },
        },
      ],
    },
  ],
  refs: REFS,
  problems: [],
  problem_count: 0,
  incomplete: null,
};

const LIST = {
  ref: REV,
  documents: [
    {
      id: 'QP-1',
      title: 'Synthetic procedure',
      doctype: 'qms.procedure',
      path: 'qms/QP-1.md',
      classification: 'internal',
      translations: [],
    },
    {
      id: 'URS-1',
      title: 'Synthetic URS',
      doctype: 'equipment.urs',
      path: 'eq/URS-1.md',
      classification: 'confidential',
      translations: ['en'],
    },
  ],
  next_after: 'URS-1',
  incomplete: null,
  withheld: null,
};

const referrer = (over: Record<string, unknown>) => ({
  lang: null,
  hidden: false,
  title: null,
  doctype: 'qms.procedure',
  path: 'qms/x.md',
  classification: 'internal',
  refs: [],
  ...over,
});

const REFERRERS = {
  ref: REV,
  id: 'RA-1',
  block: null,
  referrers: [
    referrer({
      id: 'QP-1',
      title: 'Synthetic procedure',
      refs: [
        { line: 12, block: null },
        { line: 20, block: 'H-01' },
      ],
    }),
    referrer({ id: 'QP-1', lang: 'en', title: 'Synthetic procedure (en)', refs: [{ line: 12, block: null }] }),
    referrer({ id: 'SEC-9', hidden: true, doctype: null, path: null, classification: null }),
    referrer({ id: 'SEC-9', lang: 'id', hidden: true, doctype: null, path: null, classification: null }),
  ],
  total: 4,
  next_after: null,
  incomplete: null,
};

function routes(over: Record<string, Handler> = {}) {
  return repoRoutes({
    [`GET ${API}`]: () => json(200, LIST),
    [`GET ${API}/RA-1/referenced-by`]: () => json(200, REFERRERS),
    [`GET ${API}/RA-1`]: (c) =>
      c.query.get('lang') === 'en'
        ? json(200, { ...DOC, lang: 'en', path: 'safety/RA-1.en.md', title: 'Synthetic assessment (en)' })
        : json(200, DOC),
    ...over,
  });
}

async function open(path: string, over: Record<string, Handler> = {}, lang: Language = 'en') {
  const server = mockServer(routes(over));
  const view = await renderApp({ fetch: server.fetch, path, lang });
  return { server, ...view };
}

async function violations(root: HTMLElement): Promise<string[]> {
  const result = await axe.run(root, {
    runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
    rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
  });
  return result.violations.map((v) => `${v.id}: ${v.help}`);
}

describe('document references in the text', () => {
  const words = { hidden: 'more confidential', broken: 'not found' };

  it('links a found one, gives a hidden one its id only, marks a broken one', () => {
    const out = linkRefs(DOC.sections[0]?.pieces[0]?.display ?? '', REFS as never, words);
    expect(out).toContain('[QP-1 Synthetic procedure](/-/doc-ref/QP-1)');
    expect(out).toContain('URS-1\\#REQ-2 \\(more confidential\\)');
    expect(out).toContain('QM-9 \\(not found\\)');
  });

  it('leaves code fences alone and escapes titles', () => {
    const fenced = '```\n[[QP-1]]\n```';
    expect(linkRefs(fenced, REFS as never, words)).toBe(fenced);
    const tricky = [ref({ doc: 'QP-1', target: 'found', title: '](javascript:x) *b*' })];
    const out = linkRefs('[[QP-1]]', tricky as never, words);
    expect(out).toBe('[QP-1 \\]\\(javascript:x\\) \\*b\\*](/-/doc-ref/QP-1)');
    expect(escapeMarkdown('a|b')).toBe('a\\|b');
  });

  it('reads the link target back', () => {
    expect(docRefTarget('-/doc-ref/QP-1')).toEqual({ doc: 'QP-1' });
    expect(docRefTarget('-/doc-ref/URS-1/REQ-2')).toEqual({ doc: 'URS-1', block: 'REQ-2' });
    expect(docRefTarget('docs/a.md')).toBeNull();
    expect(docRefTarget('-/doc-ref/a/b/c')).toBeNull();
  });
});

describe('a file that is a document', () => {
  it('finds the id of a front matter that names a doctype', () => {
    expect(documentId('---\ndoctype: qms.procedure\nid: QP-1\ntitle: x\n---\n# x\n')).toBe('QP-1');
    expect(documentId('---\nid: QP-1\n---\n')).toBeNull();
    expect(documentId('---\ndoctype: a\nid: ../x\n---\n')).toBeNull();
    expect(documentId('# no front matter\n')).toBeNull();
    expect(documentId(null)).toBeNull();
  });
});

describe('the document screen', () => {
  it('shows the summary, sections, blocks and references', async () => {
    const { server } = await open(`${BASE}/RA-1`);
    expect(await screen.findByRole('heading', { level: 1, name: 'Synthetic assessment' })).toBeTruthy();
    expect(server.to('GET', `${API}/RA-1`)).toHaveLength(1);
    expect(screen.getByTestId('classification').textContent).toBe('Internal');
    expect(screen.getByText('safety-team')).toBeTruthy();
    expect(screen.getByRole('heading', { level: 2, name: 'Scope' })).toBeTruthy();
    // The prose (already joined by the server) with the references drawn.
    const scope = screen.getByRole('region', { name: 'Scope' });
    const found = await within(scope).findByRole('link', { name: 'QP-1 Synthetic procedure' });
    expect(found.getAttribute('href')).toBe('/acme/forge-web/-/documents/QP-1');
    expect(within(scope).getByText(/URS-1#REQ-2 \(more confidential\)/)).toBeTruthy();
    expect(within(scope).getByText(/QM-9 \(not found\)/)).toBeTruthy();
    // Blocks of one kind: one table.
    const table = await screen.findByRole('table', { name: 'Blocks: hazard' });
    const rows = within(table).getAllByRole('row');
    expect(rows).toHaveLength(3);
    expect(within(table).getByRole('columnheader', { name: 'severity' })).toBeTruthy();
    expect(within(table).queryByRole('columnheader', { name: 'status' })).toBeNull();
    expect(within(rows[2] as HTMLElement).getByText('Deprecated')).toBeTruthy();
    expect(await within(table).findByText('A hand in the gap.')).toBeTruthy();
    // The references list.
    const refs = screen.getByRole('region', { name: 'References' });
    expect(within(refs).getByRole('link', { name: 'QP-1 Synthetic procedure' })).toBeTruthy();
    expect(
      within(refs).getByText('More confidential than this document: only its ID is shown.'),
    ).toBeTruthy();
    expect(within(refs).getByText('Not found in this repository')).toBeTruthy();
    // A translation is one link away.
    fireEvent.click(screen.getByRole('link', { name: 'en' }));
    expect(
      await screen.findByRole('heading', { level: 1, name: 'Synthetic assessment (en)' }),
    ).toBeTruthy();
    expect(server.to('GET', `${API}/RA-1`).at(-1)?.query.get('lang')).toBe('en');
  });

  it('passes the revision on', async () => {
    const { server } = await open(`${BASE}/RA-1?ref=v1.0`);
    await screen.findByRole('heading', { level: 1, name: 'Synthetic assessment' });
    expect(server.to('GET', `${API}/RA-1`)[0]?.query.get('ref')).toBe('v1.0');
    const link = await screen.findByRole('link', { name: 'All documents' });
    expect(link.getAttribute('href')).toBe('/acme/forge-web/-/documents?ref=v1.0');
  });

  it('says not found with the request id, and asks nothing for an id that is none', async () => {
    await open(`${BASE}/NOPE-1`, {
      [`GET ${API}/NOPE-1`]: () =>
        apiError(404, 'The project or repository was not found.', 'req-doc-404', 'FG-REPO-0005'),
    });
    expect(await screen.findByText('The project or repository was not found.')).toBeTruthy();
    expect(screen.getByText(/req-doc-404/)).toBeTruthy();
    const { server } = await open(`${BASE}/..%2Fx`);
    await waitFor(() => {
      expect(server.calls.some((c) => c.path === '/api/v1/projects/7')).toBe(true);
    });
    expect(server.calls.some((c) => c.path.startsWith(API))).toBe(false);
  });

  it('shows a refusal for an agent with its request id', async () => {
    await open(`${BASE}/RA-1`, {
      [`GET ${API}/RA-1`]: () =>
        apiError(403, 'This item is marked confidential.', 'req-doc-403', 'FG-AUTH-0021'),
    });
    expect(await screen.findByText('This item is marked confidential.')).toBeTruthy();
    expect(screen.getByText(/req-doc-403/)).toBeTruthy();
  });
});

describe('the referenced-by panel', () => {
  const RB = `${API}/RA-1/referenced-by`;
  const panel = () => screen.findByRole('region', { name: 'Documents that refer to this one' });
  const HIDDEN = 'You do not have access, so only its ID is shown.';

  it('lists referrers with their lines and a translation badge; a hidden one is its id only', async () => {
    const { server } = await open(`${BASE}/RA-1?ref=v1.0`);
    const section = await panel();
    const link = await within(section).findByRole('link', { name: 'QP-1 Synthetic procedure' });
    expect(link.getAttribute('href')).toBe('/acme/forge-web/-/documents/QP-1?ref=v1.0');
    expect(within(link).getByText('QP-1').tagName).toBe('CODE');
    const en = within(section).getByRole('link', { name: 'QP-1 Synthetic procedure (en)' });
    expect(en.getAttribute('href')).toBe('/acme/forge-web/-/documents/QP-1?ref=v1.0&lang=en');
    expect(within(section).getByText('Translation: en')).toBeTruthy();
    expect(within(section).getAllByText('Line 12')).toHaveLength(2);
    expect(within(section).getByText('Line 20 (to H-01)')).toBeTruthy();
    expect(within(section).getByText('4 documents refer to this document.')).toBeTruthy();
    // Hidden: the id, the referrer's own note and its language; no link, no title.
    const hidden = within(section)
      .getAllByText('SEC-9')
      .map((e) => e.closest('li') as HTMLElement);
    expect(hidden).toHaveLength(2);
    for (const li of hidden) {
      expect(li.getAttribute('data-hidden')).toBe('true');
      expect(within(li).queryByRole('link')).toBeNull();
      expect(within(li).getByText(HIDDEN)).toBeTruthy();
    }
    expect(within(hidden[1] as HTMLElement).getByText('Translation: id')).toBeTruthy();
    expect(within(section).queryByText(/More confidential than this document/)).toBeNull();
    expect(server.to('GET', RB)[0]?.query.get('ref')).toBe('v1.0');
    expect(server.unmatched).toEqual([]);
  });

  it('says what to write when nothing refers to the document', async () => {
    await open(`${BASE}/RA-1`, {
      [`GET ${RB}`]: () => json(200, { ...REFERRERS, referrers: [], total: 0 }),
    });
    const section = await panel();
    expect((await within(section).findByTestId('referrers-empty')).textContent).toBe(
      'No document refers to this one yet. Write [[RA-1]] in another document and it shows here.',
    );
    expect(within(section).queryByRole('button', { name: 'Show more' })).toBeNull();
    expect(within(section).queryByTestId('referrers-incomplete')).toBeNull();
  });

  it('keeps the list when the read was incomplete, says to reload, and shows no count for an agent', async () => {
    const { server } = await open(`${BASE}/RA-1`, {
      [`GET ${RB}`]: () =>
        json(200, {
          ...REFERRERS,
          total: null,
          incomplete: { error_code: 'FG-DOC-0002', reason: 'doc_refs' },
        }),
    });
    const section = await panel();
    const notice = await within(section).findByTestId('referrers-incomplete');
    expect(notice.textContent).toContain(
      'Some documents could not be checked, so a document may be missing here. Reload in a while.',
    );
    expect(within(notice).getByText('(FG-DOC-0002)')).toBeTruthy();
    expect(within(section).getByRole('link', { name: 'QP-1 Synthetic procedure' })).toBeTruthy();
    expect(within(section).queryByText(/refer to this document/)).toBeNull();
    fireEvent.click(within(notice).getByRole('button', { name: 'Reload' }));
    await waitFor(() => {
      expect(server.to('GET', RB)).toHaveLength(2);
    });
  });

  it('says only that not everything was checked, with a reload, when the incomplete list is empty', async () => {
    let incomplete = true;
    await open(`${BASE}/RA-1`, {
      [`GET ${RB}`]: () =>
        json(
          200,
          incomplete
            ? {
                ...REFERRERS,
                referrers: [],
                total: 0,
                incomplete: { error_code: 'FG-DOC-0002', reason: 'doc_files' },
              }
            : REFERRERS,
        ),
    });
    const section = await panel();
    const notice = await within(section).findByTestId('referrers-incomplete');
    expect(notice.textContent).toContain('Not every document could be checked. Reload in a while.');
    expect(within(section).queryByTestId('referrers-empty')).toBeNull();
    incomplete = false;
    fireEvent.click(within(notice).getByRole('button', { name: 'Reload' }));
    expect(await within(section).findByRole('link', { name: 'QP-1 Synthetic procedure' })).toBeTruthy();
    expect(within(section).queryByTestId('referrers-incomplete')).toBeNull();
  });

  it('pages on with Show more, moves the focus and announces the same words again', async () => {
    const pages: Record<string, unknown> = {
      '': { ...REFERRERS, referrers: [referrer({ id: 'A-1', title: 'First' })], next_after: 'A-1' },
      'A-1': { ...REFERRERS, referrers: [referrer({ id: 'B-1', title: 'Second' })], next_after: 'B-1' },
      'B-1': { ...REFERRERS, referrers: [referrer({ id: 'C-1', title: 'Third' })], next_after: null },
    };
    const { server } = await open(`${BASE}/RA-1`, {
      [`GET ${RB}`]: (c) => json(200, pages[c.query.get('after') ?? '']),
    });
    const section = await panel();
    fireEvent.click(await within(section).findByRole('button', { name: 'Show more' }));
    const second = await within(section).findByRole('link', { name: 'B-1 Second' });
    await waitFor(() => {
      expect(document.activeElement).toBe(second);
    });
    const live = within(section).getByTestId('referrers-announce');
    const firstSpan = live.firstElementChild;
    expect(live.textContent).toBe('1 more document is shown.');
    fireEvent.click(within(section).getByRole('button', { name: 'Show more' }));
    const third = await within(section).findByRole('link', { name: 'C-1 Third' });
    await waitFor(() => {
      expect(document.activeElement).toBe(third);
    });
    // Same words, a new node: a screen reader announces it again.
    expect(live.textContent).toBe('1 more document is shown.');
    expect(live.firstElementChild).not.toBe(firstSpan);
    expect(server.to('GET', RB).map((c) => c.query.get('after'))).toEqual([null, 'A-1', 'B-1']);
    expect(within(section).queryByRole('button', { name: 'Show more' })).toBeNull();
  });

  it('keeps the list when Show more fails and shows the request id inline', async () => {
    let fail = true;
    await open(`${BASE}/RA-1`, {
      [`GET ${RB}`]: (c) =>
        c.query.get('after') === null
          ? json(200, { ...REFERRERS, next_after: 'SEC-9.id' })
          : fail
            ? apiError(500, 'The documents could not be read.', 'req-refby-more', 'FG-DOC-0001')
            : json(200, {
                ...REFERRERS,
                referrers: [referrer({ id: 'WI-4', title: 'Later' })],
                next_after: null,
              }),
    });
    const section = await panel();
    fireEvent.click(await within(section).findByRole('button', { name: 'Show more' }));
    expect(await within(section).findByText('The documents could not be read.')).toBeTruthy();
    expect(within(section).getByText(/req-refby-more/)).toBeTruthy();
    expect(within(section).getByRole('link', { name: 'QP-1 Synthetic procedure' })).toBeTruthy();
    fail = false;
    fireEvent.click(within(section).getByRole('button', { name: /try again/i }));
    const later = await within(section).findByRole('link', { name: 'WI-4 Later' });
    await waitFor(() => {
      expect(document.activeElement).toBe(later);
    });
  });

  it('starts over for another document', async () => {
    const { server } = await open(`${BASE}/RA-1`, {
      [`GET ${RB}`]: () => json(200, { ...REFERRERS, next_after: 'SEC-9.id' }),
      [`GET ${API}/QP-1`]: () => json(200, { ...DOC, id: 'QP-1', title: 'Synthetic procedure' }),
      [`GET ${API}/QP-1/referenced-by`]: () =>
        json(200, { ...REFERRERS, id: 'QP-1', referrers: [], total: 0 }),
    });
    const section = await panel();
    fireEvent.click(await within(section).findByRole('link', { name: 'QP-1 Synthetic procedure' }));
    await screen.findByRole('heading', { level: 1, name: 'Synthetic procedure' });
    const next = await panel();
    expect(await within(next).findByTestId('referrers-empty')).toBeTruthy();
    expect(server.to('GET', `${API}/QP-1/referenced-by`)[0]?.query.get('after')).toBeNull();
    expect(within(next).getByTestId('referrers-announce').textContent).toBe('');
  });

  it('shows a first-read failure with its request id and retries', async () => {
    let fail = true;
    const { server } = await open(`${BASE}/RA-1`, {
      [`GET ${RB}`]: () =>
        fail
          ? apiError(500, 'The documents could not be read.', 'req-refby-500', 'FG-DOC-0001')
          : json(200, REFERRERS),
    });
    const section = await panel();
    expect(await within(section).findByText('The documents could not be read.')).toBeTruthy();
    expect(within(section).getByText(/req-refby-500/)).toBeTruthy();
    // The document itself is still shown.
    expect(screen.getByRole('heading', { level: 1, name: 'Synthetic assessment' })).toBeTruthy();
    fail = false;
    fireEvent.click(within(section).getByRole('button', { name: /try again/i }));
    expect(await within(section).findByRole('link', { name: 'QP-1 Synthetic procedure' })).toBeTruthy();
    expect(server.to('GET', RB).length).toBeGreaterThan(1);
  });
});

describe('the document list', () => {
  it('lists documents with their classification and pages on', async () => {
    const { server } = await open(BASE);
    const table = await screen.findByRole('table', { name: 'Documents of the project' });
    expect(within(table).getByRole('link', { name: 'QP-1' }).getAttribute('href')).toBe(
      '/acme/forge-web/-/documents/QP-1',
    );
    expect(within(table).getByText('Confidential')).toBeTruthy();
    const next = screen.getByRole('link', { name: 'Next page' });
    expect(next.getAttribute('href')).toBe('/acme/forge-web/-/documents?after=URS-1');
    fireEvent.click(next);
    await waitFor(() => {
      expect(server.to('GET', API).at(-1)?.query.get('after')).toBe('URS-1');
    });
  });

  it('says so when empty and when an agent was not given some', async () => {
    await open(BASE, {
      [`GET ${API}`]: () =>
        json(200, {
          ...LIST,
          documents: [],
          next_after: null,
          withheld: { error_code: 'FG-AUTH-0021', documents: 2, requirements: 0, work_items: 0, files: 0 },
        }),
    });
    expect(await screen.findByTestId('docs-empty')).toBeTruthy();
    expect(screen.getByText('2 confidential documents are not shown to an AI agent.')).toBeTruthy();
  });

  it('the project navigation (the sidebar) links to the documents', async () => {
    await open(BASE);
    const nav = await screen.findByRole('navigation', { name: 'Project menu' });
    const link = within(nav).getByRole('link', { name: 'Documents' });
    expect(link.getAttribute('aria-current')).toBe('page');
  });
});

describe('document screens are accessible', () => {
  for (const lang of ['ja', 'en', 'id'] as const) {
    it(`has no axe violations (${lang})`, async () => {
      const { container } = await open(`${BASE}/RA-1`, {}, lang);
      await screen.findByRole('heading', { level: 1, name: 'Synthetic assessment' });
      await screen.findAllByRole('table');
      await screen.findByText('A hand in the gap.');
      await within(screen.getByTestId('referrers')).findAllByRole('listitem');
      expect(await violations(container)).toEqual([]);
      const list = await open(BASE, {}, lang);
      await within(list.container).findAllByRole('table');
      expect(await violations(list.container)).toEqual([]);
    });
  }
});
