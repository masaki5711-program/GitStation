// MR9 review leftovers (MR10 step 0): unique ids, a save that a second Esc
// cannot cancel, focus kept on the submit button while saving.
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { I18nextProvider } from 'react-i18next';
import { describe, expect, it } from 'vitest';

import { ErrorNotice } from '../src/components/ErrorNotice';
import { createI18n } from '../src/i18n';
import { apiError, mockServer, page, signedIn } from './mock';
import { json, renderApp } from './render';

const lists = () => ({
  ...signedIn(),
  'GET /api/v1/projects': () => page([]),
  'GET /api/v1/groups': () => page([]),
  'GET /api/v1/namespaces/resolve': () =>
    json(200, { canonical_path: 'taro', kind: 'user', moved: false, namespace_id: 55, user_id: 5 }),
});

describe('MR9 review leftovers', () => {
  it('two error notices on one page have different title ids that their sections point at', async () => {
    const i18n = await createI18n('ja');
    const failure = { kind: 'network' } as const;
    const { container } = render(
      <I18nextProvider i18n={i18n}>
        <ErrorNotice failure={failure} />
        <ErrorNotice failure={failure} />
      </I18nextProvider>,
    );
    const ids = [...container.querySelectorAll('h2')].map((h) => h.id);
    expect(new Set(ids).size).toBe(2);
    for (const section of container.querySelectorAll('section')) {
      const labelledby = section.getAttribute('aria-labelledby') ?? '';
      expect(container.querySelector(`[id="${labelledby}"]`)).not.toBeNull();
    }
  });

  it('a native close while saving (second Esc in Chromium) shows the dialog again', async () => {
    let release: (r: Response) => void = () => undefined;
    const server = mockServer({
      ...lists(),
      'POST /api/v1/groups': () =>
        new Promise<Response>((resolve) => {
          release = resolve;
        }),
    });
    await renderApp({ fetch: server.fetch });
    fireEvent.click(await screen.findByRole('button', { name: 'グループを作る' }));
    const dialog = screen.getByRole<HTMLDialogElement>('dialog');
    let shown = 0;
    dialog.showModal = () => {
      shown += 1;
      dialog.setAttribute('open', '');
    };
    fireEvent.change(within(dialog).getByLabelText('名前'), { target: { value: 'Press' } });
    fireEvent.click(within(dialog).getByRole('button', { name: 'グループを作る' }));
    await waitFor(() => {
      expect(within(dialog).getByRole('button', { name: '閉じる' }).hasAttribute('disabled')).toBe(true);
    });
    dialog.removeAttribute('open');
    dialog.dispatchEvent(new Event('close'));
    expect(shown).toBe(1);
    expect(dialog.hasAttribute('open')).toBe(true);
    release(apiError(409, 'もう使われています。', 'req-dup'));
    await within(dialog).findByText('もう使われています。');
  });

  it('while saving the submit button keeps the focus (aria-disabled) and a second click sends nothing', async () => {
    let release: (r: Response) => void = () => undefined;
    const server = mockServer({
      ...lists(),
      'POST /api/v1/groups': () =>
        new Promise<Response>((resolve) => {
          release = resolve;
        }),
    });
    await renderApp({ fetch: server.fetch });
    fireEvent.click(await screen.findByRole('button', { name: 'グループを作る' }));
    const dialog = screen.getByRole('dialog');
    fireEvent.change(within(dialog).getByLabelText('名前'), { target: { value: 'Press' } });
    const submit = within(dialog).getByRole('button', { name: 'グループを作る' });
    submit.focus();
    fireEvent.click(submit);
    await waitFor(() => {
      expect(server.to('POST', '/api/v1/groups')).toHaveLength(1);
    });
    const busy = within(dialog).getByRole('button', { name: /作っています/ });
    expect(busy.hasAttribute('disabled')).toBe(false);
    expect(busy.getAttribute('aria-disabled')).toBe('true');
    expect(document.activeElement).toBe(busy);
    fireEvent.click(busy);
    expect(server.to('POST', '/api/v1/groups')).toHaveLength(1);
    release(apiError(409, 'もう使われています。', 'req-dup'));
    await within(dialog).findByText('もう使われています。');
  });
});
