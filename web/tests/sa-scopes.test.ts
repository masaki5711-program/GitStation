// The scope lists the token forms offer: `admin` is never one of them (it is
// added for an administrator's personal token by Tokens.tsx itself, and the
// server refuses it for a service account).
import { describe, expect, it } from 'vitest';

import { PERSONAL_SCOPES, SERVICE_ACCOUNT_SCOPES } from '../src/pages/settings/scopes';

describe('token scope lists', () => {
  it('never contain admin', () => {
    expect(PERSONAL_SCOPES as readonly string[]).not.toContain('admin');
    expect(SERVICE_ACCOUNT_SCOPES as readonly string[]).not.toContain('admin');
  });

  it('a service account may get every personal scope', () => {
    for (const s of PERSONAL_SCOPES) expect(SERVICE_ACCOUNT_SCOPES).toContain(s);
  });
});
