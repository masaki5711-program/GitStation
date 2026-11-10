// Plan 13 Q13: the project's document lint migration in the settings. A
// Maintainer sets the date with a date field bounded by the server's dates
// (tomorrow .. max_until), sees "migrating: until" while it is in force and
// "ended" past it, and ends it now; a Developer reads it only. The browser's
// clock decides nothing: today / max_until come from the read. axe in every
// language.
import axe from 'axe-core';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import type { Language } from '../src/i18n';
import { cutoff, nextDay } from '../src/pages/settings/DocLintMigration';
import { docLintMigration, settingsRoutes } from './merge-settings-fixtures';
import { apiError, mockServer, SESSION, type Handler } from './mock';
import { json, renderApp } from './render';

const API = '/api/v1/projects/7/doc-lint/migration';
const PATH = '/acme/forge-web/-/settings';
const MANAGER = { can_manage: true };

async function open(over: Record<string, Handler> = {}, lang: Language = 'en') {
  const server = mockServer(settingsRoutes(over));
  const view = await renderApp({ fetch: server.fetch, path: PATH, lang });
  await screen.findByRole('heading', { level: 1 });
  return { server, ...view };
}

const section = () => screen.findByRole('region', { name: 'Document checks during a migration' });

describe('document lint migration', () => {
  it('computes the next day by the calendar, in UTC', () => {
    expect(nextDay('2026-11-10')).toBe('2026-11-11');
    expect(nextDay('2026-12-31')).toBe('2027-01-01');
    expect(nextDay('2028-02-28')).toBe('2028-02-29');
  });

  it('shows when a date ends in the viewer\'s own time zone (the end of the day in UTC)', () => {
    // 2026-12-01 ends at 2026-12-02 00:00 UTC = 09:00 in Tokyo, 19:00 on Dec 1 in New York.
    const tokyo = cutoff('2026-12-01', 'en', 'Asia/Tokyo');
    expect(tokyo).toContain('Dec 2, 2026');
    expect(tokyo).toContain('9:00');
    const york = cutoff('2026-12-01', 'en', 'America/New_York');
    expect(york).toContain('Dec 1, 2026');
    expect(york).toContain('7:00');
    expect(cutoff('2026-12-01', 'ja', 'Asia/Tokyo')).toContain('2026年12月2日 9:00');
  });

  it('a Maintainer sets the date with the CSRF header; the field is bounded by the server dates', async () => {
    const { server } = await open({
      [`GET ${API}`]: () => json(200, docLintMigration({ viewer: MANAGER })),
      [`PUT ${API}`]: (c) =>
        json(
          200,
          docLintMigration({
            warn_until: (c.body as { warn_until: string }).warn_until,
            in_force: true,
            set_by: 5,
            set_at: '2026-11-10T01:02:03.000000Z',
            viewer: MANAGER,
          }),
        ),
    });
    const s = await section();
    expect(within(s).getByText('Broken document references are errors.')).toBeTruthy();
    const field = await within(s).findByLabelText<HTMLInputElement>('Warnings until (UTC)');
    expect(field.type).toBe('date');
    expect(field.min).toBe('2026-11-11');
    expect(field.max).toBe('2027-05-09');
    expect(field.required).toBe(true);
    // Empty: refused here, nothing sent.
    fireEvent.click(within(s).getByRole('button', { name: 'Set the migration date' }));
    expect(await within(s).findByText('Enter the last day of the migration.')).toBeTruthy();
    expect(field.getAttribute('aria-invalid')).toBe('true');
    // Past the bound: refused here too.
    fireEvent.change(field, { target: { value: '2027-06-01' } });
    fireEvent.click(within(s).getByRole('button', { name: 'Set the migration date' }));
    expect(await within(s).findByText('Enter a date from 2026-11-11 to 2027-05-09.')).toBeTruthy();
    expect(server.to('PUT', API)).toHaveLength(0);

    fireEvent.change(field, { target: { value: '2027-03-31' } });
    fireEvent.click(within(s).getByRole('button', { name: 'Set the migration date' }));
    await waitFor(() => {
      expect(server.to('PUT', API)).toHaveLength(1);
    });
    const put = server.to('PUT', API)[0];
    expect(put?.body).toEqual({ warn_until: '2027-03-31' });
    expect(put?.headers.get('X-GitStation-CSRF')).toBe(SESSION.csrf_token);
    expect(await within(s).findByText('Broken references are warnings until 2027-03-31.')).toBeTruthy();
    expect(
      within(s).getByText(
        `Migrating: broken references are warnings through the end of 2027-03-31 in UTC (until ${cutoff('2027-03-31', 'en')} your time). After that they are errors again.`,
      ),
    ).toBeTruthy();
    // The help names the cut-off of the date typed, in the viewer's time.
    expect(within(s).getByText(new RegExp(`2027-03-31 ends at ${cutoff('2027-03-31', 'en').replace(/[.*+?^${}()|[\]\\]/g, '\\$&')} your time`))).toBeTruthy();
  });

  it('ends the migration now', async () => {
    let cleared = false;
    const { server } = await open({
      [`GET ${API}`]: () =>
        json(
          200,
          cleared
            ? docLintMigration({ viewer: MANAGER })
            : docLintMigration({
                warn_until: '2026-12-01',
                in_force: true,
                set_by: 5,
                set_at: '2026-11-01T00:00:00.000000Z',
                viewer: MANAGER,
              }),
        ),
      [`DELETE ${API}`]: () => {
        cleared = true;
        return new Response(null, { status: 204 });
      },
    });
    const s = await section();
    fireEvent.click(await within(s).findByRole('button', { name: 'End the migration now' }));
    await waitFor(() => {
      expect(server.to('DELETE', API)).toHaveLength(1);
    });
    expect(await within(s).findByText('The migration ended: broken references are errors again.')).toBeTruthy();
    expect(await within(s).findByText('Broken document references are errors.')).toBeTruthy();
    expect(within(s).queryByRole('button', { name: 'End the migration now' })).toBeNull();
  });

  it('an ended date is shown as ended; a Developer reads it only', async () => {
    await open({
      [`GET ${API}`]: () =>
        json(200, docLintMigration({ warn_until: '2026-11-01', in_force: false, set_by: 5, set_at: '2026-10-01T00:00:00.000000Z' })),
    });
    const s = await section();
    expect(
      await within(s).findByText('The migration ended on 2026-11-01: broken references are errors again.'),
    ).toBeTruthy();
    expect(within(s).getByText('Only a Maintainer (or higher) can change this.')).toBeTruthy();
    expect(within(s).queryByLabelText('Warnings until (UTC)')).toBeNull();
  });

  it('a refused date shows the server message and the request id', async () => {
    await open({
      [`GET ${API}`]: () => json(200, docLintMigration({ viewer: MANAGER })),
      [`PUT ${API}`]: () =>
        apiError(400, 'The migration’s end date must be between tomorrow and 180 days from today.', 'req-q13', 'FG-DOC-0005'),
    });
    const s = await section();
    fireEvent.change(await within(s).findByLabelText('Warnings until (UTC)'), { target: { value: '2026-12-01' } });
    fireEvent.click(within(s).getByRole('button', { name: 'Set the migration date' }));
    expect(await within(s).findByText(/between tomorrow and 180 days/)).toBeTruthy();
    expect(within(s).getByText(/req-q13/)).toBeTruthy();
  });
});

async function violations(root: HTMLElement): Promise<string[]> {
  const result = await axe.run(root, {
    runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
    rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
  });
  return result.violations.map((v) => `${v.id}: ${v.help} (${v.nodes.map((n) => n.target.join(' ')).join(', ')})`);
}

describe('axe: document lint migration', () => {
  for (const lang of ['ja', 'en', 'id'] as Language[]) {
    it(`${lang}: in force, editable`, async () => {
      const { container } = await open(
        {
          [`GET ${API}`]: () =>
            json(
              200,
              docLintMigration({
                warn_until: '2026-12-01',
                in_force: true,
                set_by: 5,
                set_at: '2026-11-01T00:00:00.000000Z',
                viewer: MANAGER,
              }),
            ),
        },
        lang,
      );
      await screen.findByRole('region', { name: /移行中の文書の検査|Document checks during a migration|Pemeriksaan dokumen selama migrasi/ });
      await waitFor(() => {
        expect(screen.queryByText(/読み込んでいます|Loading|Memuat/)).toBeNull();
      });
      expect(await violations(container)).toEqual([]);
    });
  }
});
