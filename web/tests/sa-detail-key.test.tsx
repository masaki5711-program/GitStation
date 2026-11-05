// The secret of an issued token belongs to one account: when the address moves
// to another account (history, an edited URL) the secret state is dropped, not
// carried over (AccountDetail is keyed by the account id).
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import type { ReactNode } from 'react';
import { MemoryRouter, useNavigate } from 'react-router';
import { describe, expect, it } from 'vitest';

import { createApiClient } from '../src/api/client';
import { AppProviders, createQueryClient } from '../src/App';
import { createI18n } from '../src/i18n';
import { ServiceAccounts } from '../src/pages/settings/ServiceAccounts';
import { useSessionState } from '../src/session/session';
import { mockServer, SESSION } from './mock';
import { json } from './render';

const account = (id: number) => ({
  id,
  username: `sa-${id}`,
  display_name: `Account ${id}`,
  purpose: 'x',
  state: 'active',
  active_tokens: 0,
  last_token_expiry: null,
  created_at: '2026-10-01T00:00:00Z',
  created_by: 5,
  owner: { id: 5, username: 'taro', active: true },
});

function Gate({ children }: { children: ReactNode }) {
  const s = useSessionState();
  return s.status === 'authed' ? <>{children}</> : null;
}

function Jump() {
  const navigate = useNavigate();
  return (
    <button
      type="button"
      onClick={() => {
        void navigate('/s?tab=service-accounts&account=4');
      }}
    >
      jump
    </button>
  );
}

describe('account detail', () => {
  it('drops the secret when the address moves to another account', async () => {
    const routes: Record<string, () => Response> = {
      'GET /api/v1/session': () => json(200, { ...SESSION, user: { ...SESSION.user, is_admin: true } }),
      'POST /api/v1/admin/service-accounts/3/tokens': () =>
        json(201, {
          id: 1,
          public_id: 'p',
          name: 'x',
          scopes: ['read_repository'],
          expires_at: '2027-01-30T00:00:00Z',
          is_agent: false,
          issued_by_user_id: 5,
          token: 'fgp_p_SYNTHETICSECRET',
        }),
    };
    for (const id of [3, 4]) {
      routes[`GET /api/v1/admin/service-accounts/${id}`] = () => json(200, account(id));
      routes[`GET /api/v1/admin/service-accounts/${id}/tokens`] = () => json(200, { items: [] });
      routes[`GET /api/v1/admin/service-accounts/${id}/check-grants`] = () => json(200, { grants: [] });
    }
    const server = mockServer(routes);
    const i18n = await createI18n('en');
    const api = createApiClient({ language: () => i18n.language, fetch: server.fetch });
    const queryClient = createQueryClient();
    queryClient.setDefaultOptions({ queries: { retry: false } });
    render(
      <AppProviders i18n={i18n} api={api} queryClient={queryClient}>
        <MemoryRouter initialEntries={['/s?tab=service-accounts&account=3']}>
          <Gate>
            <ServiceAccounts base="/s" />
            <Jump />
          </Gate>
        </MemoryRouter>
      </AppProviders>,
    );
    await screen.findByRole('heading', { name: 'Account 3' });
    fireEvent.change(await screen.findByLabelText('Name'), { target: { value: 'x' } });
    fireEvent.click(screen.getByRole('button', { name: 'Issue token' }));
    await screen.findByTestId('token-secret');
    fireEvent.click(screen.getByRole('button', { name: 'jump' }));
    await screen.findByRole('heading', { name: 'Account 4' });
    await waitFor(() => {
      expect(screen.queryByTestId('token-secret')).toBeNull();
    });
    expect(document.body.textContent).not.toContain('SYNTHETICSECRET');
    // The issue form is back (not the "finish first" hint of the other account).
    expect(screen.getByRole('button', { name: 'Issue token' })).toBeTruthy();
  });
});
