// The web session (ADR-0021): `GET /api/v1/session` says who is signed in
// and gives the CSRF token for unsafe requests. 401 = nobody is signed in.
import { useQuery } from '@tanstack/react-query';

import { ApiFailure, call, failureOf, type Failure } from '../api/client';
import { useApi } from '../api/context';
import type { Session } from '../api/types';

export type SessionState =
  | { status: 'pending' }
  | { status: 'anonymous' }
  | { status: 'error'; failure: Failure; retry: () => void }
  | { status: 'authed'; session: Session };

export const SESSION_KEY = ['session'] as const;

export function useSessionState(): SessionState {
  const api = useApi();
  const q = useQuery({
    queryKey: SESSION_KEY,
    queryFn: async ({ signal }) => {
      const session = await call(() => api.GET('/api/v1/session', { signal }));
      api.csrf.token = session.csrf_token;
      return session;
    },
  });
  if (q.isPending) return { status: 'pending' };
  if (q.isError) {
    if (q.error instanceof ApiFailure && q.error.failure.kind === 'unauthenticated') {
      api.csrf.token = null;
      return { status: 'anonymous' };
    }
    return {
      status: 'error',
      failure: failureOf(q.error),
      retry: () => {
        void q.refetch();
      },
    };
  }
  return { status: 'authed', session: q.data };
}

/** The signed-in session; only for components under `RequireSession`. */
export function useSession(): Session {
  const state = useSessionState();
  if (state.status !== 'authed') {
    throw new Error('useSession outside a signed-in page');
  }
  return state.session;
}
