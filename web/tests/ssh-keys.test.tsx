// M4 C13: the SSH keys tab of the account settings — the list with the
// server's host key fingerprint, adding a key (checked before sending, the
// CSRF header on the POST), deleting one only after asking, and SSH off.
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { apiError, mockServer, SESSION, signedIn } from './mock';
import { json, renderApp } from './render';

const CSRF = SESSION.csrf_token;
const PATH = '/taro/-/settings?tab=ssh-keys';
const ED25519 =
  'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAGuL+srU7BTyMdIqbgh4NRqXH1vOCQWfZxL4x1ZB/uN laptop';
const FP = 'SHA256:5ytgilOc3oe+dzOfNlMNUlML18fMocF7QAyCsvhkxkA';
const SERVER = {
  clone_host: 'git.example.test:2222',
  host_key_fingerprints: ['SHA256:hostkeyhostkeyhostkeyhostkeyhostkeyhostkey0'],
};

const key = (id: number, title: string) => ({
  id,
  title,
  algorithm: 'ssh-ed25519',
  fingerprint: FP,
  public_key: ED25519.split(' ').slice(0, 2).join(' '),
  created_at: '2026-11-01T00:00:00Z',
  last_used_at: null,
});

const user = (over: Record<string, unknown> = {}) => ({
  ...signedIn(),
  'GET /api/v1/namespaces/resolve': () =>
    json(200, { canonical_path: 'taro', kind: 'user', moved: false, namespace_id: 55, user_id: 5 }),
  ...over,
});

describe('SSH keys', () => {
  it('lists the keys with the clone URL and the host key to compare', async () => {
    const server = mockServer(
      user({ 'GET /api/v1/user/ssh-keys': () => json(200, { items: [key(3, 'laptop')], ssh: SERVER }) }),
    );
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    expect(await screen.findByRole('heading', { name: 'SSH keys' })).toBeTruthy();
    expect(await screen.findByText(FP)).toBeTruthy();
    expect(screen.getByText('git clone ssh://git@git.example.test:2222/<group>/<project>.git')).toBeTruthy();
    expect(screen.getByText(SERVER.host_key_fingerprints[0] as string)).toBeTruthy();
  });

  it('says so when SSH is off', async () => {
    const server = mockServer(user({ 'GET /api/v1/user/ssh-keys': () => json(200, { items: [], ssh: null }) }));
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    expect(await screen.findByText('SSH is off on this server. Use the HTTPS URL and an access token.')).toBeTruthy();
    expect(screen.getByText('No SSH keys yet.')).toBeTruthy();
  });

  it('checks the key before sending, then adds it with the CSRF header', async () => {
    let items: unknown[] = [];
    const server = mockServer(
      user({
        'GET /api/v1/user/ssh-keys': () => json(200, { items, ssh: SERVER }),
        'POST /api/v1/user/ssh-keys': () => {
          items = [key(4, 'laptop')];
          return json(201, key(4, 'laptop'));
        },
      }),
    );
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    const box = await screen.findByLabelText('Public key');
    fireEvent.change(box, { target: { value: 'ssh-rsa AAAAB3NzaC1yc2E= old' } });
    fireEvent.click(screen.getByRole('button', { name: 'Add key' }));
    expect(
      (await screen.findAllByText('Paste one Ed25519 or ECDSA public key line (RSA is not accepted).')).length,
    ).toBeGreaterThan(0);
    expect(server.to('POST', '/api/v1/user/ssh-keys')).toHaveLength(0);
    fireEvent.change(box, { target: { value: `  ${ED25519}\n` } });
    fireEvent.click(screen.getByRole('button', { name: 'Add key' }));
    await waitFor(() => {
      expect(server.to('POST', '/api/v1/user/ssh-keys')).toHaveLength(1);
    });
    const post = server.to('POST', '/api/v1/user/ssh-keys')[0];
    expect(post?.body).toEqual({ key: ED25519, title: null });
    expect(post?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    expect(await screen.findByTestId('ssh-key-added')).toBeTruthy();
  });

  it('a refused key shows the one-line reason and the request id only', async () => {
    const server = mockServer(
      user({
        'GET /api/v1/user/ssh-keys': () => json(200, { items: [], ssh: SERVER }),
        'POST /api/v1/user/ssh-keys': () =>
          apiError(422, 'The SSH public key cannot be added.', 'req-ssh-1', 'FG-AUTH-0023'),
      }),
    );
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    fireEvent.change(await screen.findByLabelText('Public key'), { target: { value: ED25519 } });
    fireEvent.click(screen.getByRole('button', { name: 'Add key' }));
    expect(await screen.findByText('The SSH public key cannot be added.')).toBeTruthy();
    expect(screen.getByText('req-ssh-1')).toBeTruthy();
    expect(document.body.textContent).not.toContain('FG-AUTH-0023');
  });

  it('deletes a key only after asking', async () => {
    const server = mockServer(
      user({
        'GET /api/v1/user/ssh-keys': () => json(200, { items: [key(3, 'laptop')], ssh: SERVER }),
        'DELETE /api/v1/user/ssh-keys/3': () => new Response(null, { status: 204 }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    fireEvent.click(await screen.findByRole('button', { name: 'Delete laptop' }));
    expect(server.to('DELETE', '/api/v1/user/ssh-keys/3')).toHaveLength(0);
    expect(await screen.findByText('Delete this SSH key?')).toBeTruthy();
    const dialog = screen.getByRole('dialog');
    fireEvent.click(within(dialog).getByRole('button', { name: 'Delete' }));
    await waitFor(() => {
      expect(server.to('DELETE', '/api/v1/user/ssh-keys/3')).toHaveLength(1);
    });
    expect(server.to('DELETE', '/api/v1/user/ssh-keys/3')[0]?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
  });
});
