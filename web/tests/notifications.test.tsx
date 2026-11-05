// U3a: the signed-in person's notification settings (N1 preferences API):
// mail on or off, the language of the mail, the kinds muted (the kinds come
// from the API). Saved with the CSRF header; a refusal shows the reason and
// the request id; the result is announced.
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { apiError, mockServer, SESSION, signedIn, type Handler } from './mock';
import { json, renderApp } from './render';
import { PREFS, PREFS_URL } from './webhook-fixtures';

const CSRF = SESSION.csrf_token;
const PATH = '/taro/-/settings?tab=notifications';

const userRoutes = (over: Record<string, Handler> = {}): Record<string, Handler> => ({
  ...signedIn(),
  'GET /api/v1/namespaces/resolve': () =>
    json(200, { canonical_path: 'taro', kind: 'user', moved: false, namespace_id: 55, user_id: 5 }),
  [`GET ${PREFS_URL}`]: () => json(200, PREFS),
  ...over,
});

async function open(over: Record<string, Handler> = {}) {
  const server = mockServer(userRoutes(over));
  const view = await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
  await screen.findByRole('heading', { level: 2, name: 'Notifications' });
  return { server, ...view };
}

describe('notification settings', () => {
  it('is a tab of the account settings', async () => {
    await open();
    expect(screen.getByRole('link', { name: 'Notifications' }).getAttribute('aria-current')).toBe('page');
  });

  it('shows the stored choices; the kinds come from the API', async () => {
    await open();
    expect((await screen.findByLabelText<HTMLInputElement>('Send me notification mail')).checked).toBe(true);
    expect(screen.getByLabelText<HTMLSelectElement>('Language of the mail').value).toBe('');
    const group = screen.getByRole('group', { name: 'Notifications to stop' });
    // Each kind has a translated name, with its code as secondary text.
    expect(within(group).getByLabelText<HTMLInputElement>(/Merge request approved/).checked).toBe(false);
    expect(within(group).getByLabelText<HTMLInputElement>(/Merge request merged/).checked).toBe(false);
    expect(within(group).getByLabelText<HTMLInputElement>(/Comment on a merge request/).checked).toBe(true);
    expect(within(group).getByText('mr_commented')).toBeTruthy();
  });

  it('links the help texts to their controls', async () => {
    await open();
    const mail = await screen.findByLabelText('Send me notification mail');
    expect(document.getElementById(mail.getAttribute('aria-describedby') ?? '')?.textContent).toContain('Turn this off');
    const group = screen.getByRole('group', { name: 'Notifications to stop' });
    expect(document.getElementById(group.getAttribute('aria-describedby') ?? '')?.textContent).toContain('Checked kinds');
  });

  it('keeps a stored language this screen does not know', async () => {
    const { server } = await open({
      [`GET ${PREFS_URL}`]: () => json(200, { ...PREFS, language: 'de' }),
      [`PUT ${PREFS_URL}`]: () => json(200, PREFS),
    });
    const select = await screen.findByLabelText<HTMLSelectElement>('Language of the mail');
    expect(select.value).toBe('de');
    fireEvent.click(screen.getByRole('button', { name: 'Save notification settings' }));
    await waitFor(() => {
      expect(server.to('PUT', PREFS_URL)).toHaveLength(1);
    });
    expect((server.to('PUT', PREFS_URL)[0]?.body as { language: unknown }).language).toBe('de');
  });

  it('saves the changes with the CSRF header and announces it', async () => {
    const { server } = await open({
      [`PUT ${PREFS_URL}`]: (c) => json(200, { ...PREFS, ...(c.body as object) }),
    });
    fireEvent.click(await screen.findByLabelText('Send me notification mail'));
    fireEvent.change(screen.getByLabelText('Language of the mail'), { target: { value: 'ja' } });
    fireEvent.click(screen.getByLabelText(/Merge request approved/));
    fireEvent.click(screen.getByLabelText(/Comment on a merge request/));
    fireEvent.click(screen.getByRole('button', { name: 'Save notification settings' }));
    await waitFor(() => {
      expect(server.to('PUT', PREFS_URL)).toHaveLength(1);
    });
    const call = server.to('PUT', PREFS_URL)[0];
    expect(call?.body).toEqual({ email_enabled: false, language: 'ja', muted_kinds: ['mr_approved'] });
    expect(call?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    const saved = await screen.findByText('Notification settings were saved.');
    expect(saved.closest('[role="status"]')).not.toBeNull();
  });

  it('sends null for the instance default language', async () => {
    const { server } = await open({
      [`GET ${PREFS_URL}`]: () => json(200, { ...PREFS, language: 'id' }),
      [`PUT ${PREFS_URL}`]: () => json(200, PREFS),
    });
    fireEvent.change(await screen.findByLabelText('Language of the mail'), { target: { value: '' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save notification settings' }));
    await waitFor(() => {
      expect(server.to('PUT', PREFS_URL)).toHaveLength(1);
    });
    expect((server.to('PUT', PREFS_URL)[0]?.body as { language: unknown }).language).toBeNull();
  });

  it('shows a refusal with its request id and keeps the choices', async () => {
    await open({ [`PUT ${PREFS_URL}`]: () => apiError(400, 'That language is not offered.', 'req-pref-1') });
    fireEvent.click(await screen.findByLabelText(/Comment on a merge request/));
    fireEvent.click(screen.getByRole('button', { name: 'Save notification settings' }));
    expect(await screen.findByText('That language is not offered.')).toBeTruthy();
    expect(screen.getByTestId('request-id').textContent).toBe('req-pref-1');
    expect(screen.getByLabelText<HTMLInputElement>(/Comment on a merge request/).checked).toBe(false);
  });

  it('shows the failure to read with a retry', async () => {
    const server = mockServer(
      userRoutes({ [`GET ${PREFS_URL}`]: () => apiError(500, 'Settings could not be read.', 'req-pref-2') }),
    );
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    expect(await screen.findByText('Settings could not be read.')).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Try again' })).toBeTruthy();
  });
});
