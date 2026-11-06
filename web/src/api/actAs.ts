// The act-as confirmation (U3, ADR-0025 §5.4): what the popup shows
// (`GET …/act-as-confirmation`, forge's data only) and the person's press
// (`POST /-/confirm/…/assertions`, CSRF). The integration's `state` is only
// ever the body of that one POST: never a query key, a log line or storage.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { SESSION_KEY } from '../session/session';
import { ApiFailure, call } from './client';
import { useApi } from './context';
import type { components } from './schema.gen';

type S = components['schemas'];
export type ActAsConfirmation = S['ConfirmationBody'];
/** How long a pending assertion lives after the press (forge-db `LIFETIME`). */
export const ASSERTION_TTL_MINUTES = 5;
export type ActAsIntent = 'approve' | 'unapprove';

/** What the integration put into the popup's address, checked. */
export interface ConfirmLink {
  intent: ActAsIntent;
  sha: string;
  integration: number;
  state: string;
}

/** The popup's query, strictly (the server checks again): `null` when any
 * part is missing or malformed, so the screen asks the API nothing. */
export function parseConfirmLink(search: URLSearchParams): ConfirmLink | null {
  const intent = search.get('intent');
  const sha = search.get('sha');
  const integration = search.get('integration');
  const state = search.get('state');
  if (intent !== 'approve' && intent !== 'unapprove') return null;
  if (sha === null || !/^(?:[0-9a-f]{40}|[0-9a-f]{64})$/.test(sha)) return null;
  if (integration === null || !/^[1-9][0-9]{0,15}$/.test(integration)) return null;
  const id = Number(integration);
  if (!Number.isSafeInteger(id)) return null;
  if (state === null || !/^[A-Za-z0-9_-]{22,128}$/.test(state)) return null;
  return { intent, sha, integration: id, state };
}

const STATE_KEY = 'gs-act-as-state';

/** The link of this popup, once: the `state` is taken out of the address
 * (history.replaceState - it must not stay in the URL bar, history or a
 * copied link) and kept in sessionStorage for this tab only, so a reload or
 * the sign-in round trip still has it. Storage may be missing: then the
 * state lives only in the returned value. */
export function takeConfirmLink(search: URLSearchParams): ConfirmLink | null {
  const params = new URLSearchParams(search);
  const fromUrl = params.get('state');
  let state = fromUrl;
  const key = `${STATE_KEY}:${window.location.pathname}`;
  try {
    if (state === null) state = window.sessionStorage.getItem(key);
    else window.sessionStorage.setItem(key, state);
  } catch {
    // Private mode or blocked storage: the in-memory value is enough.
  }
  if (fromUrl !== null) {
    const here = new URLSearchParams(window.location.search);
    here.delete('state');
    const rest = here.toString();
    window.history.replaceState(window.history.state, '', window.location.pathname + (rest === '' ? '' : `?${rest}`) + window.location.hash);
  }
  if (state === null) return null;
  params.set('state', state);
  return parseConfirmLink(params);
}

export const actAsKeys = {
  confirmation: (project: number, iid: number, link: Omit<ConfirmLink, 'state'>) =>
    ['act-as-confirmation', project, iid, link.intent, link.sha, link.integration] as const,
};

export function useActAsConfirmation(project: number, iid: number, link: ConfirmLink | null, enabled: boolean) {
  const api = useApi();
  return useQuery({
    queryKey: actAsKeys.confirmation(project, iid, {
      intent: link?.intent ?? 'approve',
      sha: link?.sha ?? '',
      integration: link?.integration ?? 0,
    }),
    enabled: enabled && link !== null,
    // What the person sees must be what is checked now: never reuse.
    gcTime: 0,
    staleTime: 0,
    queryFn: ({ signal }) => {
      if (link === null) throw new Error('act-as confirmation without a link');
      return call(() =>
        api.GET('/api/v1/projects/{id}/merge-requests/{iid}/act-as-confirmation', {
          signal,
          params: {
            path: { id: project, iid: String(iid) },
            query: { intent: link.intent, sha: link.sha, integration: link.integration },
          },
        }),
      );
    },
  });
}

/** The press. A 403 is most often a stale CSRF token: read the session again. */
export function useIssueAssertion(project: number, iid: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  return useMutation({
    gcTime: 0,
    mutationFn: (v: { link: ConfirmLink; targetSha: string }) =>
      call(() =>
        api.POST('/-/confirm/projects/{id}/merge-requests/{iid}/assertions', {
          params: { path: { id: project, iid: String(iid) } },
          body: {
            intent: v.link.intent,
            sha: v.link.sha,
            target_sha: v.targetSha,
            integration: v.link.integration,
            state: v.link.state,
          },
        }),
      ),
    onError: (error: unknown) => {
      // 401: the session ended while the page was open - the sign-in gate
      // takes over (and returns here). 403: most often a stale CSRF token.
      if (
        error instanceof ApiFailure &&
        (error.failure.kind === 'unauthenticated' ||
          (error.failure.kind === 'api' && error.failure.status === 403))
      ) {
        void queryClient.invalidateQueries({ queryKey: SESSION_KEY });
      }
    },
  });
}

/** The press, as the screen holds it. */
export type IssueMutation = ReturnType<typeof useIssueAssertion>;
